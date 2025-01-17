use bitcoin::{
    opcodes::{all::*, OP_FALSE, OP_TRUE},
    script::Builder,
    ScriptBuf, TxOut,
};
use secp256k1::XOnlyPublicKey;

use crate::EVMAddress;

#[derive(Debug, Clone)]
pub struct ScriptBuilder {
    pub verifiers_pks: Vec<XOnlyPublicKey>,
}

impl ScriptBuilder {
    pub fn new(verifiers_pks: Vec<XOnlyPublicKey>) -> Self {
        Self { verifiers_pks }
    }

    pub fn anyone_can_spend_txout() -> TxOut {
        let script = Builder::new().push_opcode(OP_TRUE).into_script();
        let script_pubkey = script.to_p2wsh();
        let value = script_pubkey.dust_value();
        TxOut {
            script_pubkey,
            value,
        }
    }

    pub fn op_return_txout(evm_address: &EVMAddress) -> TxOut {
        let script = Builder::new()
            .push_opcode(OP_RETURN)
            .push_slice(evm_address)
            .into_script();
        let script_pubkey = script.to_p2wsh();
        let value = script_pubkey.dust_value();
        TxOut {
            script_pubkey,
            value,
        }
    }

    pub fn generate_script_n_of_n(&self) -> ScriptBuf {
        let mut builder = Builder::new();
        for vpk in self.verifiers_pks.clone() {
            builder = builder.push_x_only_key(&vpk).push_opcode(OP_CHECKSIGVERIFY);
        }
        builder = builder.push_opcode(OP_TRUE);
        builder.into_script()
    }

    pub fn generate_script_n_of_n_with_user_pk(&self, user_pk: &XOnlyPublicKey) -> ScriptBuf {
        let mut builder = Builder::new();
        for vpk in self.verifiers_pks.clone() {
            builder = builder.push_x_only_key(&vpk).push_opcode(OP_CHECKSIGVERIFY);
        }
        builder = builder
            .push_x_only_key(user_pk)
            .push_opcode(OP_CHECKSIGVERIFY);
        builder = builder.push_opcode(OP_TRUE);
        builder.into_script()
    }

    pub fn create_inscription_script_32_bytes(
        public_key: &XOnlyPublicKey,
        data: &Vec<[u8; 32]>,
    ) -> ScriptBuf {
        let mut inscribe_preimage_script_builder = Builder::new()
            .push_x_only_key(public_key)
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF);
        for elem in data {
            inscribe_preimage_script_builder = inscribe_preimage_script_builder.push_slice(elem);
        }
        inscribe_preimage_script_builder = inscribe_preimage_script_builder.push_opcode(OP_ENDIF);

        inscribe_preimage_script_builder.into_script()
    }

    // ATTENTION: If you want to spend a UTXO using timelock script, the condition is that
    // # in the script < # in the sequence of the tx < # of blocks mined after UTXO appears on the chain

    pub fn generate_timelock_script(actor_pk: &XOnlyPublicKey, block_count: u32) -> ScriptBuf {
        Builder::new()
            .push_int(block_count as i64)
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP)
            .push_x_only_key(actor_pk)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    pub fn generate_absolute_timelock_script(
        actor_pk: &XOnlyPublicKey,
        block_count: u32,
    ) -> ScriptBuf {
        Builder::new()
            .push_int(block_count as i64)
            .push_opcode(OP_CLTV)
            .push_opcode(OP_DROP)
            .push_x_only_key(actor_pk)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    pub fn generate_hash_script(hash: [u8; 32]) -> ScriptBuf {
        Builder::new()
            .push_opcode(OP_SHA256)
            .push_slice(hash)
            .push_opcode(OP_EQUAL)
            .into_script()
    }

    pub fn generate_dust_script(evm_address: &EVMAddress) -> ScriptBuf {
        Builder::new()
            .push_opcode(OP_RETURN)
            .push_slice(evm_address)
            .into_script()
    }
}
