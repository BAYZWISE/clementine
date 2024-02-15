use std::borrow::BorrowMut;
use std::collections::{HashMap, HashSet};
use std::vec;

use crate::actor::{Actor, EVMSignature};
use crate::custom_merkle::CustomMerkleTree;
use crate::extended_rpc::ExtendedRpc;
use crate::merkle::MerkleTree;
use crate::script_builder::ScriptBuilder;
use crate::transaction_builder::{self, TransactionBuilder};
use crate::utils::{calculate_amount, handle_anyone_can_spend_script, handle_taproot_witness};
use crate::verifier::Verifier;
use bitcoin::address::NetworkChecked;
use bitcoin::consensus::serialize;
use bitcoin::sighash::SighashCache;
use bitcoin::{hashes::Hash, secp256k1, secp256k1::schnorr, Address, Txid};
use bitcoin::{Amount, OutPoint, Transaction};
use bitcoincore_rpc::{Client, RpcApi};
use circuit_helpers::config::{
    BRIDGE_AMOUNT_SATS, CONNECTOR_TREE_DEPTH, CONNECTOR_TREE_OPERATOR_TAKES_AFTER,
};
use circuit_helpers::constant::{EVMAddress, DUST_VALUE, HASH_FUNCTION_32, MIN_RELAY_FEE};
use secp256k1::rand::rngs::OsRng;
use secp256k1::rand::Rng;
use secp256k1::schnorr::Signature;
use secp256k1::{All, Secp256k1, XOnlyPublicKey};
pub type PreimageType = [u8; 32];

pub fn check_deposit(
    secp: &Secp256k1<All>,
    rpc: &Client,
    start_utxo: OutPoint,
    deposit_utxo: OutPoint,
    hash: [u8; 32],
    return_address: XOnlyPublicKey,
    verifiers_pks: &Vec<XOnlyPublicKey>,
) {
    // 1. Check if tx is mined in bitcoin
    // 2. Check if the start_utxo matches input[0].previous_output
    // 2. Check if 0th output of the txid has 1 BTC
    // 3. Check if 0th output of the txid's scriptpubkey is N-of-N multisig and Hash of preimage or return_address after 200 blocks
    // 4. If all checks pass, return true
    // 5. Return the blockheight of the block in which the txid was mined
    let tx = rpc
        .get_raw_transaction(&deposit_utxo.txid, None)
        .unwrap_or_else(|e| {
            panic!(
                "Failed to get raw transaction: {}, txid: {}",
                e, deposit_utxo.txid
            )
        });
    println!("user deposit utxo: {:?}", deposit_utxo);
    assert!(tx.input[0].previous_output == start_utxo);
    println!("from user start utxo: {:?}", start_utxo);
    assert!(tx.output[deposit_utxo.vout as usize].value == Amount::from_sat(BRIDGE_AMOUNT_SATS));
    println!("amount: {:?}", tx.output[deposit_utxo.vout as usize].value);
    // let (address, _) = generate_deposit_address(secp, verifiers_pks, return_address, hash); // TODO: Update this function
    // assert!(tx.output[deposit_utxo.vout as usize].script_pubkey == address.script_pubkey());
}

pub fn create_connector_tree_preimages_and_hashes(
    depth: usize,
    rng: &mut OsRng,
) -> (Vec<Vec<PreimageType>>, Vec<Vec<[u8; 32]>>) {
    let mut connector_tree_preimages: Vec<Vec<PreimageType>> = Vec::new();
    let mut connector_tree_hashes: Vec<Vec<[u8; 32]>> = Vec::new();
    let root_preimage: PreimageType = rng.gen();
    connector_tree_preimages.push(vec![root_preimage]);
    connector_tree_hashes.push(vec![HASH_FUNCTION_32(root_preimage)]);
    for i in 1..(depth + 1) {
        let mut preimages_current_level: Vec<PreimageType> = Vec::new();
        let mut hashes_current_level: Vec<PreimageType> = Vec::new();
        for _ in 0..2u32.pow(i as u32) {
            let temp: PreimageType = rng.gen();
            preimages_current_level.push(temp);
            hashes_current_level.push(HASH_FUNCTION_32(temp));
        }
        connector_tree_preimages.push(preimages_current_level);
        connector_tree_hashes.push(hashes_current_level);
    }
    (connector_tree_preimages, connector_tree_hashes)
}

#[derive(Debug, Clone)]
pub struct DepositPresigns {
    pub rollup_sign: EVMSignature,
    pub move_sign: schnorr::Signature,
    pub operator_claim_sign: schnorr::Signature,
}

#[derive(Debug, Clone)]
pub struct Operator<'a> {
    pub rpc: &'a ExtendedRpc,
    pub signer: Actor,
    pub script_builder: ScriptBuilder,
    pub transaction_builder: TransactionBuilder,
    pub verifiers_pks: Vec<XOnlyPublicKey>,
    pub verifier_evm_addresses: Vec<EVMAddress>,
    pub deposit_presigns: HashMap<Txid, Vec<DepositPresigns>>,
    pub deposit_merkle_tree: MerkleTree,
    pub withdrawals_merkle_tree: MerkleTree,
    pub withdrawals_payment_txids: Vec<Txid>,
    pub mock_verifier_access: Vec<Verifier<'a>>, // on production this will be removed rather we will call the verifier's API
    pub preimages: Vec<PreimageType>,
    pub connector_tree_utxos: Vec<Vec<OutPoint>>,
    pub connector_tree_preimages: Vec<Vec<PreimageType>>,
    pub connector_tree_hashes: Vec<Vec<[u8; 32]>>,
    pub deposit_utxos: Vec<OutPoint>,
    pub move_utxos: Vec<OutPoint>,
    pub current_preimage_for_deposit_requests: PreimageType,
}

impl<'a> Operator<'a> {
    pub fn new(rng: &mut OsRng, rpc: &'a ExtendedRpc, num_verifier: u32) -> Self {
        let signer = Actor::new(rng);
        let (connector_tree_preimages, connector_tree_hashes) =
            create_connector_tree_preimages_and_hashes(CONNECTOR_TREE_DEPTH, rng);
        let mut verifiers = Vec::new();
        let mut verifiers_pks = Vec::new();
        for _ in 0..num_verifier {
            let verifier = Verifier::new(rng, &rpc, signer.xonly_public_key.clone());
            verifiers_pks.push(verifier.signer.xonly_public_key.clone());
            verifiers.push(verifier);
        }
        let mut all_verifiers = verifiers_pks.to_vec();
        all_verifiers.push(signer.xonly_public_key.clone());
        let script_builder = ScriptBuilder::new(all_verifiers.clone());
        let transaction_builder = TransactionBuilder::new(all_verifiers.clone());

        Self {
            rpc,
            signer,
            script_builder,
            transaction_builder,
            verifiers_pks: verifiers_pks,
            verifier_evm_addresses: Vec::new(),
            deposit_presigns: HashMap::new(),
            deposit_merkle_tree: MerkleTree::initial(),
            withdrawals_merkle_tree: MerkleTree::initial(),
            withdrawals_payment_txids: Vec::new(),
            mock_verifier_access: verifiers,
            preimages: Vec::new(),
            connector_tree_utxos: Vec::new(),
            connector_tree_preimages: connector_tree_preimages,
            connector_tree_hashes: connector_tree_hashes,
            deposit_utxos: Vec::new(),
            move_utxos: Vec::new(),
            current_preimage_for_deposit_requests: rng.gen(),
        }
    }

    pub fn change_preimage_for_deposit_requests(&mut self, rng: &mut OsRng) {
        self.current_preimage_for_deposit_requests = rng.gen();
    }

    pub fn add_deposit_utxo(&mut self, utxo: OutPoint) {
        self.deposit_utxos.push(utxo);
    }

    pub fn get_all_verifiers(&self) -> Vec<XOnlyPublicKey> {
        let mut all_verifiers = self.verifiers_pks.to_vec();
        all_verifiers.push(self.signer.xonly_public_key.clone());
        all_verifiers
    }

    pub fn set_connector_tree_utxos(&mut self, connector_tree_utxos: Vec<Vec<OutPoint>>) {
        self.connector_tree_utxos = connector_tree_utxos;
    }

    // this is a public endpoint that every depositor can call
    pub fn new_deposit(
        &mut self,
        start_utxo: OutPoint,
        index: u32,
        hash: [u8; 32],
        return_address: XOnlyPublicKey,
        evm_address: EVMAddress,
    ) -> Vec<EVMSignature> {
        // self.verifiers + signer.public_key
        let all_verifiers = self.get_all_verifiers();
        let (deposit_address, _) = self
            .transaction_builder
            .generate_deposit_address(return_address, hash);
        let deposit_tx_ins = TransactionBuilder::create_tx_ins(vec![start_utxo]);
        let deposit_tx_outs = TransactionBuilder::create_tx_outs(vec![(
            Amount::from_sat(BRIDGE_AMOUNT_SATS),
            deposit_address.script_pubkey(),
        )]);
        let deposit_tx = TransactionBuilder::create_btc_tx(deposit_tx_ins, deposit_tx_outs);
        let deposit_txid = deposit_tx.txid();
        let deposit_utxo = TransactionBuilder::create_utxo(deposit_txid, 0);
        // println!("all_verifiers checking: {:?}", all_verifiers);
        // println!("mock verifier access: {:?}", self.mock_verifier_access);
        let presigns_from_all_verifiers = self
            .mock_verifier_access
            .iter()
            .map(|verifier| {
                // println!("verifier in the closure: {:?}", verifier);
                // Note: In this part we will need to call the verifier's API to get the presigns
                let deposit_presigns = verifier.new_deposit(
                    start_utxo,
                    Amount::from_sat(BRIDGE_AMOUNT_SATS),
                    index,
                    hash,
                    return_address.clone(),
                    evm_address,
                    &all_verifiers,
                    self.signer.address.clone(),
                );
                println!("checked new deposit");
                // check_presigns(deposit_utxo, &deposit_presigns);
                println!("checked presigns");
                deposit_presigns
            })
            .collect::<Vec<_>>();
        println!("presigns_from_all_verifiers: done");

        let (anyone_can_spend_script_pub_key, _) = handle_anyone_can_spend_script();

        let move_tx = TransactionBuilder::create_move_tx(
            vec![deposit_utxo],
            vec![
                (
                    Amount::from_sat(BRIDGE_AMOUNT_SATS)
                        - Amount::from_sat(DUST_VALUE)
                        - Amount::from_sat(MIN_RELAY_FEE),
                    self.script_builder.generate_n_of_n_script_without_hash(),
                ),
                (
                    Amount::from_sat(DUST_VALUE),
                    anyone_can_spend_script_pub_key,
                ),
            ],
        );

        let move_txid = move_tx.txid();

        let rollup_sign = self.signer.sign_deposit(deposit_txid, evm_address, hash);
        let mut all_rollup_signs = presigns_from_all_verifiers
            .iter()
            .map(|presigns| presigns.rollup_sign)
            .collect::<Vec<_>>();
        all_rollup_signs.push(rollup_sign);
        self.deposit_presigns
            .insert(deposit_utxo.txid, presigns_from_all_verifiers);
        println!("inserted deposit presigns for: {:?}", deposit_utxo.txid);
        all_rollup_signs
    }

    // this is called when a Withdrawal event emitted on rollup
    pub fn new_withdrawal(&mut self, withdrawal_address: Address<NetworkChecked>) {
        let taproot_script = withdrawal_address.script_pubkey();
        // we are assuming that the withdrawal_address is a taproot address so we get the last 32 bytes
        let hash: [u8; 34] = taproot_script.as_bytes().try_into().unwrap();
        let hash: [u8; 32] = hash[2..].try_into().unwrap();

        // 1. Add the address to WithdrawalsMerkleTree
        self.withdrawals_merkle_tree.add(hash);

        // self.withdrawals_merkle_tree.add(withdrawal_address.to);

        // 2. Pay to the address and save the txid
        let txid = self
            .rpc
            .send_to_address(&withdrawal_address, 100_000_000)
            .txid;
        println!(
            "operator paid to withdrawal address: {:?}, txid: {:?}",
            withdrawal_address, txid
        );
        self.withdrawals_payment_txids.push(txid);
    }

    // this is called start utxo is spent and deposit utxo is created
    pub fn deposit_happened(
        &mut self,
        start_utxo: OutPoint,
        hash: [u8; 32],
        deposit_utxo: OutPoint,
        return_address: XOnlyPublicKey, // TODO: SAVE THIS TO STRUCT
    ) -> OutPoint {
        check_deposit(
            &self.signer.secp,
            &self.rpc.inner,
            start_utxo,
            deposit_utxo,
            hash,
            return_address.clone(),
            &self.get_all_verifiers(),
        );
        // 1. Add the corresponding txid to DepositsMerkleTree
        self.deposit_merkle_tree
            .add(deposit_utxo.txid.to_byte_array());
        let preimage = self.current_preimage_for_deposit_requests.clone();
        let hash = HASH_FUNCTION_32(preimage);
        let all_verifiers = self.get_all_verifiers();
        let script_n_of_n = self.script_builder.generate_n_of_n_script(hash);

        let script_n_of_n_without_hash = self.script_builder.generate_n_of_n_script_without_hash();
        let (address, _) = TransactionBuilder::create_taproot_address(
            &self.signer.secp,
            vec![script_n_of_n_without_hash.clone()],
        );
        println!("address while taking deposit: {:?}", address);
        println!(
            "address.script_pubkey() while taking deposit: {:?}",
            address.script_pubkey()
        );

        let mut move_tx = TransactionBuilder::create_move_tx(
            vec![deposit_utxo],
            vec![(
                Amount::from_sat(BRIDGE_AMOUNT_SATS) - Amount::from_sat(MIN_RELAY_FEE),
                address.script_pubkey(),
            )],
        );
        println!("move_tx is from: {:?}", deposit_utxo);
        self.add_deposit_utxo(deposit_utxo);

        let (deposit_address, deposit_taproot_info) = self
            .transaction_builder
            .generate_deposit_address(return_address, hash);

        let prevouts = TransactionBuilder::create_tx_outs(vec![(
            Amount::from_sat(BRIDGE_AMOUNT_SATS),
            deposit_address.script_pubkey(),
        )]);

        let mut move_signatures: Vec<Signature> = Vec::new();
        let deposit_presigns_for_move = self
            .deposit_presigns
            .get(&deposit_utxo.txid)
            .expect("Deposit presigns not found");
        for presign in deposit_presigns_for_move.iter() {
            move_signatures.push(presign.move_sign);
        }

        let sig =
            self.signer
                .sign_taproot_script_spend_tx(&mut move_tx, prevouts, &script_n_of_n, 0);
        move_signatures.push(sig);
        move_signatures.reverse();

        let mut witness_elements: Vec<&[u8]> = Vec::new();
        witness_elements.push(&preimage);
        for sig in move_signatures.iter() {
            witness_elements.push(sig.as_ref());
        }

        handle_taproot_witness(
            &mut move_tx,
            0,
            witness_elements,
            script_n_of_n,
            deposit_taproot_info,
        );

        // println!("witness size: {:?}", witness.size());
        // println!("kickoff_tx: {:?}", kickoff_tx);

        let rpc_move_txid = self.rpc.inner.send_raw_transaction(&move_tx).unwrap();
        println!("rpc_move_txid: {:?}", rpc_move_txid);
        let move_utxo = TransactionBuilder::create_utxo(rpc_move_txid, 0);
        self.move_utxos.push(move_utxo.clone());
        move_utxo
    }

    pub fn create_child_pays_for_parent(&self, parent_outpoint: OutPoint) -> Transaction {
        let resource_utxo = self
            .rpc
            .send_to_address(&self.signer.address, BRIDGE_AMOUNT_SATS);
        let resource_tx = self
            .rpc
            .get_raw_transaction(&resource_utxo.txid, None)
            .unwrap();

        let all_verifiers = self.get_all_verifiers();

        let script_n_of_n_without_hash = self.script_builder.generate_n_of_n_script_without_hash();
        let (address, _) = TransactionBuilder::create_taproot_address(
            &self.signer.secp,
            vec![script_n_of_n_without_hash.clone()],
        );

        let (anyone_can_spend_script_pub_key, _) = handle_anyone_can_spend_script();

        let child_tx_ins = TransactionBuilder::create_tx_ins(vec![parent_outpoint, resource_utxo]);

        let child_tx_outs = TransactionBuilder::create_tx_outs(vec![
            (
                Amount::from_sat(BRIDGE_AMOUNT_SATS)
                    - Amount::from_sat(DUST_VALUE)
                    - Amount::from_sat(MIN_RELAY_FEE),
                address.script_pubkey(),
            ),
            (
                Amount::from_sat(DUST_VALUE),
                anyone_can_spend_script_pub_key.clone(),
            ),
        ]);

        let mut child_tx = TransactionBuilder::create_btc_tx(child_tx_ins, child_tx_outs);

        child_tx.input[0].witness.push([0x51]);

        let prevouts = TransactionBuilder::create_tx_outs(vec![
            (
                Amount::from_sat(DUST_VALUE),
                anyone_can_spend_script_pub_key,
            ),
            (
                Amount::from_sat(BRIDGE_AMOUNT_SATS),
                self.signer.address.script_pubkey(),
            ),
        ]);
        let sig = self
            .signer
            .sign_taproot_pubkey_spend_tx(&mut child_tx, prevouts, 1);
        let mut sighash_cache = SighashCache::new(child_tx.borrow_mut());
        let witness = sighash_cache.witness_mut(1).unwrap();
        witness.push(sig.as_ref());
        // println!("child_tx: {:?}", child_tx);
        // println!("child_txid: {:?}", child_tx.txid());
        child_tx
    }

    // this function is internal, where it checks if the current bitcoin height reaced to th end of the period,
    pub fn period1_end(&self) {
        // self.move_bridge_funds();

        // Check if all deposists are satisifed, all remaning bridge funds are moved to a new multisig
    }

    // this function is internal, where it checks if the current bitcoin height reaced to th end of the period,
    pub fn period2_end(&self) {
        // This is the time we generate proof.
    }

    // this function is internal, where it checks if the current bitcoin height reaced to th end of the period,
    pub fn period3_end(&self) {
        // This is the time send generated proof along with k-deep proof
        // and revealing bit-commitments for the next bitVM instance.
    }

    // This function is internal, it gives the appropriate response for a bitvm challenge
    pub fn challenge_received() {}

    pub fn spend_connector_tree_utxo(
        &self,
        utxo: OutPoint,
        preimage: PreimageType,
        tree_depth: usize,
    ) {
        let hash = HASH_FUNCTION_32(preimage);
        let (_, tree_info) = TransactionBuilder::create_connector_tree_node_address(
            &self.signer.secp,
            self.signer.xonly_public_key,
            hash,
        );

        let base_tx = match self.rpc.get_raw_transaction(&utxo.txid, None) {
            Ok(txid) => Some(txid),
            Err(e) => {
                eprintln!("Failed to get raw transaction: {}", e);
                None
            }
        };
        println!("base_tx: {:?}", base_tx);

        if base_tx.is_none() {
            return;
        }
        let depth = u32::ilog2(
            ((base_tx.unwrap().output[utxo.vout as usize].value.to_sat() + MIN_RELAY_FEE)
                / (DUST_VALUE + MIN_RELAY_FEE)) as u32,
        );
        println!("depth: {:?}", depth);
        let level = tree_depth - depth as usize;
        //find the index of preimage in the connector_tree_preimages[level as usize]
        let index = self.connector_tree_preimages[level as usize]
            .iter()
            .position(|x| *x == preimage)
            .unwrap();
        let hashes = (
            self.connector_tree_hashes[(level + 1) as usize][2 * index],
            self.connector_tree_hashes[(level + 1) as usize][2 * index + 1],
        );

        let utxo_tx = self.rpc.get_raw_transaction(&utxo.txid, None).unwrap();
        // println!("utxo_tx: {:?}", utxo_tx);
        // println!("utxo_txid: {:?}", utxo_tx.txid());
        let timelock_script =
            ScriptBuilder::generate_timelock_script(self.signer.xonly_public_key, 1);

        let (first_address, _) = TransactionBuilder::create_connector_tree_node_address(
            &self.signer.secp,
            self.signer.xonly_public_key,
            hashes.0,
        );

        let (second_address, _) = TransactionBuilder::create_connector_tree_node_address(
            &self.signer.secp,
            self.signer.xonly_public_key,
            hashes.1,
        );

        let mut tx = TransactionBuilder::create_connector_tree_tx(
            &utxo,
            depth as usize - 1,
            first_address,
            second_address,
        );
        // println!("created spend tx: {:?}", tx);

        let sig = self.signer.sign_taproot_script_spend_tx(
            &mut tx,
            vec![utxo_tx.output[utxo.vout as usize].clone()],
            &timelock_script,
            0,
        );
        // let spend_control_block = tree_info
        //     .control_block(&(timelock_script.clone(), LeafVersion::TapScript))
        //     .expect("Cannot create control block");
        // let mut sighash_cache = SighashCache::new(tx.borrow_mut());
        // let witness = sighash_cache.witness_mut(0).unwrap();
        // witness.push(sig.as_ref());
        // witness.push(timelock_script);
        // witness.push(&spend_control_block.serialize());

        let mut witness_elements: Vec<&[u8]> = Vec::new();
        witness_elements.push(sig.as_ref());

        handle_taproot_witness(&mut tx, 0, witness_elements, timelock_script, tree_info);

        let bytes_tx = serialize(&tx);
        // println!("bytes_connector_tree_tx length: {:?}", bytes_connector_tree_tx.len());
        // let hex_utxo_tx = hex::encode(bytes_utxo_tx.clone());
        let spending_txid = match self.rpc.send_raw_transaction(&bytes_tx) {
            Ok(txid) => Some(txid),
            Err(e) => {
                eprintln!("Failed to send raw transaction: {}", e);
                None
            }
        };
        println!("operator_spending_txid: {:?}", spending_txid);
    }

    pub fn reveal_connector_tree_preimages(
        &self,
        number_of_funds_claim: u32,
    ) -> HashSet<PreimageType> {
        let indices = CustomMerkleTree::get_indices(
            self.connector_tree_hashes.len() - 1,
            number_of_funds_claim,
        );
        println!("indices: {:?}", indices);
        let mut preimages: HashSet<PreimageType> = HashSet::new();
        for (depth, index) in indices {
            preimages.insert(self.connector_tree_preimages[depth as usize][index as usize]);
        }
        preimages
    }

    pub fn inscribe_connector_tree_preimages(&self, number_of_funds_claim: u32) -> (Txid, Txid) {
        let indices = CustomMerkleTree::get_indices(
            self.connector_tree_hashes.len() - 1,
            number_of_funds_claim,
        );
        println!("indices: {:?}", indices);
        let mut preimages: Vec<PreimageType> = Vec::new();

        for (depth, index) in indices {
            preimages.push(self.connector_tree_preimages[depth as usize][index as usize]);
        }

        let inscription_source_utxo = self
            .rpc
            .send_to_address(&self.signer.address, DUST_VALUE * 3);
        let (commit_tx, reveal_tx) = TransactionBuilder::create_inscription_transactions(
            &self.signer,
            inscription_source_utxo,
            preimages,
        );
        let commit_txid = self
            .rpc
            .send_raw_transaction(&serialize(&commit_tx))
            .unwrap();
        println!("commit_txid: {:?}", commit_txid);
        let reveal_txid = self
            .rpc
            .send_raw_transaction(&serialize(&reveal_tx))
            .unwrap();
        println!("reveal_txid: {:?}", reveal_txid);
        return (commit_txid, reveal_txid);
    }

    pub fn claim_deposit(&self, index: usize) {
        let preimage = self.connector_tree_preimages.last().unwrap()[index];
        let hash = HASH_FUNCTION_32(preimage);
        let (address, tree_info_1) = TransactionBuilder::create_connector_tree_node_address(
            &self.signer.secp,
            self.signer.xonly_public_key,
            hash,
        );
        // println!("deposit_utxos: {:?}", self.deposit_utxos);
        let deposit_utxo = self.deposit_utxos[index as usize];
        let fund_utxo = self.move_utxos[index as usize];
        let connector_utxo = self.connector_tree_utxos.last().unwrap()[index as usize];

        let mut tx_ins = TransactionBuilder::create_tx_ins(vec![fund_utxo]);
        tx_ins.extend(TransactionBuilder::create_tx_ins_with_sequence(vec![
            connector_utxo,
        ]));

        let tx_outs = TransactionBuilder::create_tx_outs(vec![(
            Amount::from_sat(BRIDGE_AMOUNT_SATS) + Amount::from_sat(DUST_VALUE)
                - Amount::from_sat(MIN_RELAY_FEE) * 2,
            self.signer.address.script_pubkey(),
        )]);

        let mut claim_tx = TransactionBuilder::create_btc_tx(tx_ins, tx_outs);

        println!("operator ready to send claim_tx: {:?}", claim_tx);

        let all_verifiers = self.get_all_verifiers();

        let script_n_of_n_without_hash = self.script_builder.generate_n_of_n_script_without_hash();
        let (multisig_address, tree_info_0) = TransactionBuilder::create_taproot_address(
            &self.signer.secp,
            vec![script_n_of_n_without_hash.clone()],
        );

        let timelock_script = ScriptBuilder::generate_timelock_script(
            self.signer.xonly_public_key,
            CONNECTOR_TREE_OPERATOR_TAKES_AFTER as u32,
        );

        let prevouts = TransactionBuilder::create_tx_outs(vec![
            (
                Amount::from_sat(BRIDGE_AMOUNT_SATS) - Amount::from_sat(MIN_RELAY_FEE),
                multisig_address.script_pubkey(),
            ),
            (Amount::from_sat(DUST_VALUE), address.script_pubkey()),
        ]);
        // println!("multisig address: {:?}", multisig_address);
        // println!(
        //     "multisig script pubkey: {:?}",
        //     multisig_address.script_pubkey()
        // );

        // let spend_control_block0 = tree_info_0
        //     .control_block(&(script_n_of_n_without_hash.clone(), LeafVersion::TapScript))
        //     .expect("Cannot create control block");

        // let spend_control_block1 = tree_info_1
        //     .control_block(&(timelock_script.clone(), LeafVersion::TapScript))
        //     .expect("Cannot create control block");

        let sig0 = self.signer.sign_taproot_script_spend_tx(
            &mut claim_tx,
            prevouts.clone(),
            &script_n_of_n_without_hash,
            0,
        );
        // let mut claim_sigs = self.mock_verifier_access.iter().map(|verifier|
        //     verifier.signer.sign_taproot_script_spend_tx(&mut claim_tx, prevouts.clone(), &script_n_of_n_without_hash, 0)
        // ).collect::<Vec<_>>();

        // println!("claim_sigs: {:?}", claim_sigs);

        let sig_1 =
            self.signer
                .sign_taproot_script_spend_tx(&mut claim_tx, prevouts, &timelock_script, 1);

        // let mut sighash_cache = SighashCache::new(claim_tx.borrow_mut());
        let sig_vec = self.deposit_presigns.get(&deposit_utxo.txid).unwrap();

        // let witness0 = sighash_cache.witness_mut(0).unwrap();
        let mut claim_sigs = sig_vec
            .iter()
            .map(|presig| presig.operator_claim_sign)
            .collect::<Vec<_>>();
        // println!("claim_sigs: {:?}", claim_sigs);
        claim_sigs.push(sig0);
        claim_sigs.reverse();
        // for sig in claim_sigs.iter() {
        //     witness0.push(sig.as_ref());
        // }
        // witness0.push(script_n_of_n_without_hash.clone());
        // witness0.push(&spend_control_block0.serialize());

        let mut witness_elements_0: Vec<&[u8]> = Vec::new();
        for sig in claim_sigs.iter() {
            witness_elements_0.push(sig.as_ref());
        }
        handle_taproot_witness(
            &mut claim_tx,
            0,
            witness_elements_0,
            script_n_of_n_without_hash,
            tree_info_0,
        );

        let mut witness_elements_1: Vec<&[u8]> = Vec::new();
        witness_elements_1.push(sig_1.as_ref());
        handle_taproot_witness(
            &mut claim_tx,
            1,
            witness_elements_1,
            timelock_script,
            tree_info_1,
        );

        // println!("deposit_utxo.txid: {:?}", deposit_utxo.txid);
        // let witness1 = sighash_cache.witness_mut(1).unwrap();
        // witness1.push(sig_1.as_ref());
        // witness1.push(timelock_script);
        // witness1.push(&spend_control_block1.serialize());

        // println!("claim_tx: {:?}", claim_tx);
        let tx_bytes = serialize(&claim_tx);
        let txid = match self.rpc.send_raw_transaction(&tx_bytes) {
            Ok(txid) => Some(txid),
            Err(e) => {
                eprintln!("Failed to send raw transaction: {}", e);
                None
            }
        };
        if txid.is_none() {
            println!("claim failed");
            return;
        } else {
            println!("claim successful, txid: {:?}", txid);
        }
    }

    pub fn create_connector_root(&mut self) -> OutPoint {
        let total_amount = calculate_amount(
            CONNECTOR_TREE_DEPTH,
            Amount::from_sat(DUST_VALUE),
            Amount::from_sat(MIN_RELAY_FEE),
        );
        let (root_address, _) = TransactionBuilder::create_connector_tree_node_address(
            &self.signer.secp,
            self.signer.xonly_public_key,
            self.connector_tree_hashes[0][0],
        );
        let root_utxo = self
            .rpc
            .send_to_address(&root_address, total_amount.to_sat());

        let utxo_tree = self.transaction_builder.create_connector_binary_tree(
            self.signer.xonly_public_key,
            root_utxo,
            CONNECTOR_TREE_DEPTH,
            self.connector_tree_hashes.clone(),
        );

        self.set_connector_tree_utxos(utxo_tree.clone());
        root_utxo
    }
}

#[cfg(test)]
mod tests {

    use std::collections::{HashMap, HashSet};

    use bitcoin::{Amount, OutPoint};
    use bitcoincore_rpc::{Auth, Client, RpcApi};
    use circuit_helpers::{
        bitcoin::{get_script_hash, verify_script_hash_taproot_address},
        config::{BRIDGE_AMOUNT_SATS, CONNECTOR_TREE_DEPTH, NUM_USERS, NUM_VERIFIERS},
        constant::{DUST_VALUE, HASH_FUNCTION_32, MIN_RELAY_FEE},
    };
    use secp256k1::rand::rngs::OsRng;

    use crate::{
        extended_rpc::ExtendedRpc,
        operator::{Operator, PreimageType},
        transaction_builder::TransactionBuilder,
        user::User,
        utils::calculate_amount,
    };

    #[test]
    fn test_connector_tree_tx() {
        let mut bridge_funds: Vec<bitcoin::Txid> = Vec::new();
        let rpc = ExtendedRpc::new();

        let total_amount = calculate_amount(
            CONNECTOR_TREE_DEPTH,
            Amount::from_sat(DUST_VALUE),
            Amount::from_sat(MIN_RELAY_FEE),
        );
        let mut operator = Operator::new(&mut OsRng, &rpc, NUM_VERIFIERS as u32);
        let mut users = Vec::new();
        for _ in 0..NUM_USERS {
            users.push(User::new(&mut OsRng, &rpc.inner));
        }
        let verifiers_pks = operator.get_all_verifiers();
        for verifier in &mut operator.mock_verifier_access {
            verifier.set_verifiers(verifiers_pks.clone());
        }
        println!("verifiers_pks.len: {:?}", verifiers_pks.len());
        let mut verifiers_evm_addresses = operator.verifier_evm_addresses.clone();
        verifiers_evm_addresses.push(operator.signer.evm_address);
        let mut start_utxo_vec = Vec::new();
        let mut return_addresses = Vec::new();

        let connector_root_utxo = operator.create_connector_root();
        for verifier in &mut operator.mock_verifier_access {
            verifier.connector_root_utxo_created(
                operator.connector_tree_hashes.clone(),
                connector_root_utxo,
            );
        }

        let mut preimages_verifier_track: HashSet<PreimageType> = HashSet::new();
        let mut utxos_verifier_track: HashMap<OutPoint, (u32, u32)> = HashMap::new();
        utxos_verifier_track.insert(connector_root_utxo, (0, 0));

        let mut flag = operator.mock_verifier_access[0]
            .did_connector_tree_process_start(connector_root_utxo.clone());
        println!("flag: {:?}", flag);
        if flag {
            operator.mock_verifier_access[0].watch_connector_tree(
                operator.signer.xonly_public_key,
                &mut preimages_verifier_track,
                &mut utxos_verifier_track,
            );
        }

        let mut fund_utxos = Vec::new();

        for i in 0..NUM_USERS {
            let user = &users[i];
            let (start_utxo, _) = user.create_start_utxo(
                &rpc.inner,
                Amount::from_sat(BRIDGE_AMOUNT_SATS) + Amount::from_sat(MIN_RELAY_FEE),
            );
            let hash = HASH_FUNCTION_32(operator.current_preimage_for_deposit_requests);

            let signatures = operator.new_deposit(
                start_utxo,
                i as u32,
                hash,
                user.signer.xonly_public_key.clone(),
                user.signer.evm_address,
            );

            rpc.mine_blocks(1);

            let (user_deposit_utxo, return_address) = user.deposit_tx(
                &user.rpc,
                start_utxo,
                Amount::from_sat(BRIDGE_AMOUNT_SATS),
                &user.secp,
                verifiers_pks.clone(),
                hash,
            );
            bridge_funds.push(user_deposit_utxo.txid);
            return_addresses.push(return_address);
            start_utxo_vec.push(start_utxo);
            rpc.mine_blocks(1);
            let fund =
                operator.deposit_happened(start_utxo, hash, user_deposit_utxo, return_addresses[i]);
            fund_utxos.push(fund);
            operator.change_preimage_for_deposit_requests(&mut OsRng);
        }

        flag = operator.mock_verifier_access[0]
            .did_connector_tree_process_start(connector_root_utxo.clone());
        println!("flag: {:?}", flag);
        if flag {
            operator.mock_verifier_access[0].watch_connector_tree(
                operator.signer.xonly_public_key,
                &mut preimages_verifier_track,
                &mut utxos_verifier_track,
            );
        }

        println!("utxos verifier track: {:?}", utxos_verifier_track);
        println!("preimages verifier track: {:?}", preimages_verifier_track);

        rpc.mine_blocks(3);

        let preimages = operator.reveal_connector_tree_preimages(3);
        let (commit_txid, reveal_txid) = operator.inscribe_connector_tree_preimages(3);
        println!("preimages revealed: {:?}", preimages);
        preimages_verifier_track = preimages.clone();
        let inscription_tx = operator.mock_verifier_access[0]
            .rpc
            .get_raw_transaction(&reveal_txid, None)
            .unwrap();
        println!("verifier reads inscription tx: {:?}", inscription_tx);

        let commit_tx = operator.mock_verifier_access[0]
            .rpc
            .get_raw_transaction(&commit_txid, None)
            .unwrap();
        println!("verifier reads commit tx: {:?}", commit_tx);
        let inscription_script_pubkey = &commit_tx.output[0].script_pubkey;
        let inscription_address_bytes: [u8; 32] = inscription_script_pubkey.as_bytes()[2..]
            .try_into()
            .unwrap();
        println!(
            "inscription address in bytes: {:?}",
            inscription_address_bytes
        );

        let witness_array = inscription_tx.input[0].witness.to_vec();
        println!("witness_array: {:?}", witness_array[1]);
        let inscribed_data = witness_array[1][36..witness_array[1].len() - 1].to_vec();
        println!("inscribed_data: {:?}", inscribed_data);
        println!("inscribed_data length: {:?}", inscribed_data.len());
        let mut verifier_got_preimages = Vec::new();
        for i in 0..(inscribed_data.len() / 33) {
            let preimage: [u8; 32] = inscribed_data[i * 33 + 1..(i + 1) * 33].try_into().unwrap();
            verifier_got_preimages.push(preimage);
        }

        println!("verifier_got_preimages: {:?}", verifier_got_preimages);

        let flattened_preimages: Vec<u8> = verifier_got_preimages
            .iter()
            .flat_map(|array| array.iter().copied())
            .collect();

        let flattened_slice: &[u8] = &flattened_preimages;

        // let mut test_hasher_1 = Sha256::new();
        // test_hasher_1.update([1u8]);
        // test_hasher_1.update([2u8]);
        // let test_hash_1: [u8; 32] = test_hasher_1.finalize().try_into().unwrap();
        // println!("test_hash_1: {:?}", test_hash_1);
        // let mut test_hasher_2 = Sha256::new();
        // test_hasher_2.update([1u8, 2u8]);
        // let test_hash_2: [u8; 32] = test_hasher_2.finalize().try_into().unwrap();
        // println!("test_hash_2: {:?}", test_hash_2);

        let calculated_merkle_root = get_script_hash(
            operator.signer.xonly_public_key.serialize(),
            flattened_slice,
            2,
        );
        println!("calculated_merkle_root: {:?}", calculated_merkle_root);
        let test_res = verify_script_hash_taproot_address(
            operator.signer.xonly_public_key.serialize(),
            flattened_slice,
            2,
            calculated_merkle_root,
            inscription_address_bytes,
        );
        println!("test_res: {:?}", test_res);

        for (i, utxo_level) in operator.connector_tree_utxos
            [0..operator.connector_tree_utxos.len() - 1]
            .iter()
            .enumerate()
        {
            for (j, utxo) in utxo_level.iter().enumerate() {
                let preimage = operator.connector_tree_preimages[i][j];
                println!("preimage: {:?}", preimage);
                operator.spend_connector_tree_utxo(*utxo, preimage, CONNECTOR_TREE_DEPTH);
                operator.mock_verifier_access[0].watch_connector_tree(
                    operator.signer.xonly_public_key,
                    &mut preimages_verifier_track,
                    &mut utxos_verifier_track,
                );
                println!("utxos verifier track: {:?}", utxos_verifier_track);
                println!("preimages verifier track: {:?}", preimages_verifier_track);
            }
            rpc.mine_blocks(1);
        }

        operator.mock_verifier_access[0].watch_connector_tree(
            operator.signer.xonly_public_key,
            &mut preimages_verifier_track,
            &mut utxos_verifier_track,
        );
        println!("utxos verifier track: {:?}", utxos_verifier_track);
        println!("preimages verifier track: {:?}", preimages_verifier_track);

        // for (i, utxo_to_claim_with) in utxo_tree[utxo_tree.len() - 1].iter().enumerate() {

        //         let preimage = operator.connector_tree_preimages[utxo_tree.len() - 1][i];
        //         println!("preimage: {:?}", preimage);
        //         operator.claim_deposit(i as u32);
        // }

        rpc.mine_blocks(2);

        for i in 0..NUM_USERS {
            operator.claim_deposit(i);
        }
    }
}
