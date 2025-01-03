use std::{collections::HashMap, sync::Arc, time::SystemTime};

use parking_lot::{Mutex, RwLock};
use rand::RngCore;
pub use receipts::QueryStatus as ReceiptStatus;
use receipts::ReceiptPool;
use secp256k1::SecretKey;
use tap_core::{receipt::Receipt as TapReceipt, signed_message::EIP712SignedMessage};
use thegraph_core::{
    alloy::{
        dyn_abi::Eip712Domain,
        hex,
        primitives::{Address, U256},
        signers::local::PrivateKeySigner,
    },
    AllocationId,
};

/// A receipt for an indexer request.
#[derive(Debug, Clone)]
pub enum Receipt {
    Legacy(u128, Vec<u8>),
    Tap(EIP712SignedMessage<TapReceipt>),
}

impl Receipt {
    /// Returns the value of the receipt.
    pub fn grt_value(&self) -> u128 {
        match self {
            Receipt::Legacy(value, _) => *value,
            Receipt::Tap(receipt) => receipt.message.value,
        }
    }

    /// Returns the allocation ID of the receipt.
    pub fn allocation(&self) -> Address {
        match self {
            Receipt::Legacy(_, receipt) => Address::from_slice(&receipt[0..20]),
            Receipt::Tap(receipt) => receipt.message.allocation_id,
        }
    }

    /// Serializes the receipt to a string.
    // TODO: Move to a typed header. This code should be agnostic from the serialization format.
    pub fn serialize(&self) -> String {
        match self {
            Receipt::Legacy(_, receipt) => hex::encode(&receipt[..(receipt.len() - 32)]),
            Receipt::Tap(receipt) => serde_json::to_string(&receipt).unwrap(),
        }
    }

    /// Returns the header name for the receipt.
    // TODO: Move to a typed header. This code should be agnostic from the http headers.
    pub fn header_name(&self) -> &'static str {
        match self {
            Receipt::Legacy(_, _) => "Scalar-Receipt",
            Receipt::Tap(_) => "Tap-Receipt",
        }
    }
}

/// Scalar TAP signer.
struct TapSigner {
    signer: PrivateKeySigner,
    domain: Eip712Domain,
}

impl TapSigner {
    /// Creates a new `TapSigner`.
    fn new(signer: PrivateKeySigner, chain_id: U256, verifying_contract: Address) -> Self {
        Self {
            signer,
            domain: Eip712Domain {
                name: Some("TAP".into()),
                version: Some("1".into()),
                chain_id: Some(chain_id),
                verifying_contract: Some(verifying_contract),
                salt: None,
            },
        }
    }

    /// Creates a new receipt for the given allocation and fee.
    fn create_receipt(
        &self,
        allocation: AllocationId,
        fee: u128,
    ) -> anyhow::Result<EIP712SignedMessage<TapReceipt>> {
        // Nonce generated with CSPRNG (ChaCha12), to avoid collision with receipts generated by
        // other gateway processes.
        // See https://docs.rs/rand/latest/rand/rngs/index.html#our-generators.
        let nonce = rand::thread_rng().next_u64();

        let timestamp_ns = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .try_into()
            .map_err(|_| anyhow::anyhow!("failed to convert timestamp to ns"))?;

        let receipt = TapReceipt {
            allocation_id: allocation.0 .0.into(),
            timestamp_ns,
            nonce,
            value: fee,
        };
        let signed = EIP712SignedMessage::new(&self.domain, receipt, &self.signer)
            .map_err(|e| anyhow::anyhow!("failed to sign receipt: {:?}", e))?;

        Ok(signed)
    }
}

/// Legacy Scalar signer.
struct LegacySigner {
    secret_key: &'static SecretKey,
    // Note: We are holding on to receipt pools indefinitely. This is acceptable, since the memory
    // cost is minor and the typical duration of an allocation is 28 days.
    receipt_pools: RwLock<HashMap<AllocationId, Arc<Mutex<ReceiptPool>>>>,
}

impl LegacySigner {
    /// Creates a new `LegacySigner`.
    fn new(secret_key: &'static SecretKey) -> Self {
        Self {
            secret_key,
            receipt_pools: RwLock::default(),
        }
    }

    /// Creates a new receipt for the given allocation and fee.
    fn create_receipt(
        &self,
        allocation: AllocationId,
        fee: u128,
    ) -> anyhow::Result<(u128, Vec<u8>)> {
        // Get the pool for the allocation
        let receipt_pool = self.receipt_pools.read().get(&allocation).cloned();

        // If the pool for the allocation exists, use it. Otherwise, create a new pool.
        let receipt = match receipt_pool {
            Some(pool) => {
                let mut pool = pool.lock();
                pool.commit(self.secret_key, fee.into())
            }
            None => {
                let mut pool = ReceiptPool::new(allocation.0 .0);
                let receipt = pool.commit(self.secret_key, fee.into());

                let mut write_guard = self.receipt_pools.write();
                write_guard.insert(allocation, Arc::new(Mutex::new(pool)));

                receipt
            }
        }
        .map_err(|e| anyhow::anyhow!("failed to sign legacy receipt: {:?}", e))?;

        Ok((fee, receipt))
    }

    /// Record the receipt status and release it from the pool.
    fn record_receipt(&self, allocation: &AllocationId, receipt: &[u8], status: ReceiptStatus) {
        let legacy_pool = self.receipt_pools.read();
        if let Some(legacy_pool) = legacy_pool.get(allocation) {
            legacy_pool.lock().release(receipt, status);
        };
    }
}

/// ReceiptSigner is responsible for creating receipts for indexing requests.
pub struct ReceiptSigner {
    tap: TapSigner,
    legacy: LegacySigner,
}

impl ReceiptSigner {
    /// Creates a new `ReceiptSigner`.
    pub fn new(
        signer: PrivateKeySigner,
        chain_id: U256,
        verifier: Address,
        legacy_signer: &'static SecretKey,
    ) -> Self {
        Self {
            tap: TapSigner::new(signer, chain_id, verifier),
            legacy: LegacySigner::new(legacy_signer),
        }
    }

    /// Creates a new Scalar TAP receipt for the given allocation and fee.
    pub fn create_receipt(&self, allocation: AllocationId, fee: u128) -> anyhow::Result<Receipt> {
        self.tap.create_receipt(allocation, fee).map(Receipt::Tap)
    }

    /// Creates a new Scalar legacy receipt for the given allocation and fee.
    pub fn create_legacy_receipt(
        &self,
        allocation: AllocationId,
        fee: u128,
    ) -> anyhow::Result<Receipt> {
        self.legacy
            .create_receipt(allocation, fee)
            .map(|(fee, receipt)| Receipt::Legacy(fee, receipt))
    }

    /// Record the receipt status and release it from the pool.
    pub fn record_receipt(
        &self,
        allocation: &AllocationId,
        receipt: &Receipt,
        status: ReceiptStatus,
    ) {
        if let Receipt::Legacy(_, receipt) = receipt {
            self.legacy.record_receipt(allocation, receipt, status);
        }
    }
}

#[cfg(test)]
mod tests {
    use thegraph_core::{
        allocation_id,
        alloy::{primitives::address, signers::local::PrivateKeySigner},
    };

    use super::*;

    mod legacy {
        use thegraph_core::allocation_id;

        use super::*;

        #[test]
        fn create_receipt() {
            //* Given
            let secret_key = Box::leak(Box::new(
                SecretKey::from_slice(&[0xcd; 32]).expect("invalid secret key"),
            ));

            let signer = LegacySigner::new(secret_key);

            // let indexer = address!("0xbdfb5ee5a2abf4fc7bb1bd1221067aef7f9de491");
            // let deployment = deployment_id!("QmaqcZxm6gcgWhWpQ88YKDm1keJDMpNxNGwtEDvjrjjNKh");
            let largest_allocation = allocation_id!("89b23fea4e46d40e8a4c6cca723e2a03fdd4bec2");
            let fee = 1000;

            //* When
            let res = signer.create_receipt(largest_allocation, fee);

            //* Then
            let receipt = res.expect("failed to create legacy receipt");

            assert_eq!(receipt.0, fee);
            assert!(!receipt.1.is_empty());
        }

        #[test]
        fn create_receipt_with_preexisting_pool() {
            //* Given
            let secret_key = Box::leak(Box::new(
                SecretKey::from_slice(&[0xcd; 32]).expect("invalid secret key"),
            ));

            let signer = LegacySigner::new(secret_key);

            let largest_allocation = allocation_id!("89b23fea4e46d40e8a4c6cca723e2a03fdd4bec2");
            let fee = 1000;

            // Pre-condition: Create a receipt so the pool for the allocation exists
            let _ = signer.create_receipt(largest_allocation, fee);

            //* When
            let res = signer.create_receipt(largest_allocation, fee);

            //* Then
            let receipt = res.expect("failed to create legacy receipt");

            assert_eq!(receipt.0, fee);
            assert!(!receipt.1.is_empty());
        }
    }

    mod tap {
        use thegraph_core::{
            allocation_id,
            alloy::{primitives::address, signers::local::PrivateKeySigner},
        };

        use super::*;

        #[test]
        fn create_receipt() {
            //* Given
            let secret_key = PrivateKeySigner::from_slice(&[0xcd; 32]).expect("invalid secret key");
            let signer = TapSigner::new(
                secret_key,
                1.try_into().expect("invalid chain id"),
                address!("177b557b12f22bb17a9d73dcc994d978dd6f5f89"),
            );

            let allocation = allocation_id!("89b23fea4e46d40e8a4c6cca723e2a03fdd4bec2");
            let fee = 1000;

            //* When
            let res = signer.create_receipt(allocation, fee);

            //* Then
            let receipt = res.expect("failed to create tap receipt");

            assert_eq!(receipt.message.value, fee);
        }
    }

    #[test]
    fn create_legacy_receipt() {
        //* Given
        let tap_signer = PrivateKeySigner::from_slice(&[0xcd; 32]).expect("invalid secret key");
        let legacy_secret_key = Box::leak(Box::new(
            SecretKey::from_slice(&[0xcd; 32]).expect("invalid secret key"),
        ));

        let signer = ReceiptSigner::new(
            tap_signer,
            1.try_into().expect("invalid chain id"),
            allocation_id!("177b557b12f22bb17a9d73dcc994d978dd6f5f89").into_inner(),
            legacy_secret_key,
        );

        let largest_allocation = allocation_id!("89b23fea4e46d40e8a4c6cca723e2a03fdd4bec2");
        let fee = 1000;

        //* When
        let res = signer.create_legacy_receipt(largest_allocation, fee);

        //* Then
        let receipt = res.expect("failed to create legacy receipt");
        assert!(matches!(receipt, Receipt::Legacy(_, _)));
    }

    #[test]
    fn create_tap_receipt() {
        //* Given
        let tap_signer = PrivateKeySigner::from_slice(&[0xcd; 32]).expect("invalid secret key");
        let legacy_secret_key = Box::leak(Box::new(
            SecretKey::from_slice(&[0xcd; 32]).expect("invalid secret key"),
        ));

        let signer = ReceiptSigner::new(
            tap_signer,
            1.try_into().expect("invalid chain id"),
            address!("177b557b12f22bb17a9d73dcc994d978dd6f5f89"),
            legacy_secret_key,
        );

        let largest_allocation = allocation_id!("89b23fea4e46d40e8a4c6cca723e2a03fdd4bec2");
        let fee = 1000;

        //* When
        let res = signer.create_receipt(largest_allocation, fee);

        //* Then
        let receipt = res.expect("failed to create tap receipt");
        assert!(matches!(receipt, Receipt::Tap(_)));
    }
}
