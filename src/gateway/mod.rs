mod async_result;
mod error;
mod pubkey;

#[cfg(feature = "desktop")]
use std::time::Duration;

pub use async_result::*;
use cached::proc_macro::cached;
pub use error::*;
#[cfg(feature = "web")]
use gloo_storage::{LocalStorage, Storage};
use ore::{
    state::{Bus, Proof, Treasury},
    utils::AccountDeserialize,
    BUS_ADDRESSES, TREASURY_ADDRESS,
};
use ore_types::{response::GetTransfersResponse, Transfer};
pub use pubkey::*;
#[cfg(feature = "desktop")]
use solana_account_decoder::parse_token::UiTokenAccount;
#[cfg(feature = "desktop")]
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig},
    rpc_response::RpcTokenAccountBalance,
};
use solana_client_wasm::utils::rpc_config::RpcSimulateTransactionConfig;
#[cfg(feature = "web")]
use solana_client_wasm::{
    solana_sdk::{
        clock::Clock,
        commitment_config::{CommitmentConfig, CommitmentLevel},
        compute_budget::ComputeBudgetInstruction,
        instruction::Instruction,
        pubkey::Pubkey,
        signature::{Keypair, Signature},
        signer::Signer,
        sysvar,
        transaction::Transaction,
    },
    utils::{rpc_config::RpcSendTransactionConfig, rpc_response::RpcTokenAccountBalance},
    WasmClient,
};
#[cfg(feature = "web")]
use solana_extra_wasm::{
    account_decoder::parse_token::UiTokenAccount,
    program::{
        spl_associated_token_account::{
            get_associated_token_address, instruction::create_associated_token_account,
        },
        spl_memo, spl_token,
    },
    transaction_status::{TransactionConfirmationStatus, UiTransactionEncoding},
};
#[cfg(feature = "desktop")]
use solana_sdk::{
    clock::Clock,
    commitment_config::{CommitmentConfig, CommitmentLevel},
    compute_budget::ComputeBudgetInstruction,
    instruction::{Instruction, InstructionError},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    sysvar,
    transaction::{Transaction, TransactionError},
};
#[cfg(feature = "desktop")]
use solana_transaction_status::UiTransactionEncoding;
#[cfg(feature = "desktop")]
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
#[cfg(feature = "web")]
use web_time::Duration;

use crate::metrics::{track, AppEvent};

pub const API_URL: &str = "https://ore-api-lthm.onrender.com";
pub const RPC_URL: &str = "https://rpc.ironforge.network/mainnet?apiKey=01HTD8PPGDM1JBVQVEVJKXZ47F";

pub const CU_LIMIT_CLAIM: u32 = 11_000;
pub const CU_LIMIT_RESET: u32 = 12_200;
pub const CU_LIMIT_MINE: u32 = 3200;

const RPC_RETRIES: usize = 0;
const GATEWAY_RETRIES: usize = 4;
const CONFIRM_RETRIES: usize = 4;
const SIMULATION_RETRIES: usize = 4;
const DEFAULT_PRIORITY_FEE: u64 = 1000;

pub struct Gateway {
    #[cfg(feature = "web")]
    pub rpc: WasmClient,
    #[cfg(feature = "desktop")]
    pub rpc: RpcClient,
    api_url: String,
}

impl Gateway {
    pub fn new(api_url: String, rpc_url: String) -> Self {
        Gateway {
            api_url,
            #[cfg(feature = "web")]
            rpc: WasmClient::new(&rpc_url),
            #[cfg(feature = "desktop")]
            rpc: RpcClient::new(rpc_url),
        }
    }

    pub async fn get_clock(&self) -> GatewayResult<Clock> {
        let data = self
            .rpc
            .get_account_data(&sysvar::clock::ID)
            .await
            .map_err(GatewayError::from)?;
        bincode::deserialize::<Clock>(&data).or(Err(GatewayError::FailedDeserialization))
    }

    pub async fn get_proof(&self, authority: Pubkey) -> GatewayResult<Proof> {
        let data = self
            .rpc
            .get_account_data(&proof_pubkey(authority))
            .await
            .map_err(GatewayError::from)?;
        Ok(*Proof::try_from_bytes(&data).expect("Failed to parse proof"))
    }

    pub async fn get_bus(&self, id: usize) -> GatewayResult<Bus> {
        let bus_address = BUS_ADDRESSES.get(id).unwrap();
        let data = self
            .rpc
            .get_account_data(bus_address)
            .await
            .map_err(GatewayError::from)?;
        Ok(*Bus::try_from_bytes(&data).expect("Failed to parse bus"))
    }

    pub async fn get_treasury(&self) -> GatewayResult<Treasury> {
        let data = self
            .rpc
            .get_account_data(&TREASURY_ADDRESS)
            .await
            .map_err(GatewayError::from)?;
        Ok(*Treasury::try_from_bytes(&data).expect("Failed to parse treasury account"))
    }

    pub async fn get_token_account(
        &self,
        pubkey: &Pubkey,
    ) -> GatewayResult<Option<UiTokenAccount>> {
        self.rpc
            .get_token_account(pubkey)
            .await
            .map_err(GatewayError::from)
    }

    pub async fn get_token_largest_accounts(
        &self,
        pubkey: &Pubkey,
    ) -> GatewayResult<Vec<RpcTokenAccountBalance>> {
        self.rpc
            .get_token_largest_accounts(pubkey)
            .await
            .map_err(GatewayError::from)
    }

    pub async fn send_and_confirm(
        &self,
        ixs: &[Instruction],
        dynamic_cus: bool,
        skip_confirm: bool,
    ) -> GatewayResult<Signature> {
        let signer = signer();
        let (mut hash, mut slot) = self
            .rpc
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .await
            .map_err(GatewayError::from)?;
        let mut send_cfg = RpcSendTransactionConfig {
            skip_preflight: true,
            preflight_commitment: Some(CommitmentLevel::Confirmed),
            encoding: Some(UiTransactionEncoding::Base64),
            max_retries: Some(RPC_RETRIES),
            min_context_slot: Some(slot),
        };

        // Build tx
        let mut tx = Transaction::new_with_payer(ixs, Some(&signer.pubkey()));

        // Simulate tx, if necessary
        if dynamic_cus {
            let mut sim_attempts = 0;
            'simulate: loop {
                let sim_res = self
                    .rpc
                    .simulate_transaction_with_config(
                        &tx,
                        RpcSimulateTransactionConfig {
                            sig_verify: false,
                            replace_recent_blockhash: true,
                            commitment: Some(CommitmentConfig::confirmed()),
                            encoding: Some(UiTransactionEncoding::Base64),
                            accounts: None,
                            min_context_slot: None,
                        },
                    )
                    .await;
                match sim_res {
                    Ok(sim_res) => {
                        #[cfg(feature = "desktop")]
                        let sim_res = sim_res.value;
                        if let Some(err) = sim_res.err {
                            println!("Simulaton error: {:?}", err);
                            sim_attempts += 1;
                            if sim_attempts.gt(&SIMULATION_RETRIES) {
                                return Err(GatewayError::SimulationFailed);
                            }
                        } else if let Some(units_consumed) = sim_res.units_consumed {
                            println!("Dynamic CUs: {:?}", units_consumed);
                            let cu_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(
                                units_consumed as u32 + 1000,
                            );
                            let cu_price_ix = ComputeBudgetInstruction::set_compute_unit_price(
                                DEFAULT_PRIORITY_FEE,
                            );
                            let mut final_ixs = vec![];
                            final_ixs.extend_from_slice(&[cu_budget_ix, cu_price_ix]);
                            final_ixs.extend_from_slice(ixs);
                            tx = Transaction::new_with_payer(&final_ixs, Some(&signer.pubkey()));
                            break 'simulate;
                        }
                    }
                    Err(err) => {
                        println!("Simulaton error: {:?}", err);
                        sim_attempts += 1;
                        if sim_attempts.gt(&SIMULATION_RETRIES) {
                            return Err(GatewayError::SimulationFailed);
                        }
                    }
                }
            }
        }

        // Submit tx
        tx.sign(&[&signer], hash);
        let mut sigs = vec![];
        let mut attempts = 0;
        loop {
            log::info!("Attempt: {:?}", attempts);
            match self.rpc.send_transaction_with_config(&tx, send_cfg).await {
                Ok(sig) => {
                    sigs.push(sig);
                    log::info!("{:?}", sig);

                    // Confirm tx
                    if skip_confirm {
                        return Ok(sig);
                    }
                    for _ in 0..CONFIRM_RETRIES {
                        match self.rpc.get_signature_statuses(&sigs).await {
                            Ok(signature_statuses) => {
                                log::info!("Sig status: {:?}", signature_statuses);
                                for signature_status in signature_statuses {
                                    if let Some(signature_status) = signature_status.as_ref() {
                                        if signature_status.confirmation_status.is_some() {
                                            let current_commitment = signature_status
                                                .confirmation_status
                                                .as_ref()
                                                .unwrap();
                                            log::info!("Commitment: {:?}", current_commitment);
                                            match current_commitment {
                                                TransactionConfirmationStatus::Processed => {}
                                                TransactionConfirmationStatus::Confirmed
                                                | TransactionConfirmationStatus::Finalized => {
                                                    log::info!("Confirmed: true");
                                                    return Ok(sig);
                                                }
                                            }
                                        } else {
                                            log::info!("No status");
                                        }
                                    }
                                }
                            }

                            // Handle confirmation errors
                            Err(err) => {
                                log::error!("Error confirming: {:?}", err);
                                // TODO
                            }
                        }
                        async_std::task::sleep(Duration::from_millis(2000)).await;
                    }
                    log::info!("Confirmed: false");
                }

                // Handle submit errors
                Err(err) => {
                    log::error!("Error {:?}", err);
                }
            }

            // Retry
            async_std::task::sleep(Duration::from_millis(2000)).await;
            (hash, slot) = self
                .rpc
                .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
                .await
                .map_err(GatewayError::from)?;
            send_cfg = RpcSendTransactionConfig {
                skip_preflight: true,
                preflight_commitment: Some(CommitmentLevel::Confirmed),
                encoding: Some(UiTransactionEncoding::Base64),
                max_retries: Some(RPC_RETRIES),
                min_context_slot: Some(slot),
            };
            tx.sign(&[&signer], hash);
            attempts += 1;
            if attempts > GATEWAY_RETRIES {
                return Err(GatewayError::TransactionTimeout);
            }
        }
    }

    // Ore
    pub async fn register_ore(&self) -> GatewayResult<()> {
        // Return early, if account is already initialized
        let signer = signer();
        let proof_address = proof_pubkey(signer.pubkey());
        if self.rpc.get_account(&proof_address).await.is_ok() {
            return Ok(());
        }

        // Sign and send transaction.
        let ix = ore::instruction::register(signer.pubkey());
        match self.send_and_confirm(&[ix], true, false).await {
            Ok(_) => {
                track(AppEvent::Register, None);
                Ok(())
            }
            Err(_) => Err(GatewayError::FailedRegister),
        }
    }

    pub async fn claim_ore(&self, amount: u64) -> GatewayResult<Signature> {
        let signer = signer();
        let beneficiary = ore_token_account_address(signer.pubkey());
        let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(CU_LIMIT_CLAIM);
        let cu_price_ix = ComputeBudgetInstruction::set_compute_unit_price(DEFAULT_PRIORITY_FEE);
        let ix = ore::instruction::claim(signer.pubkey(), beneficiary, amount);
        self.send_and_confirm(&[cu_limit_ix, cu_price_ix, ix], false, false)
            .await
    }

    pub async fn transfer_ore(
        &self,
        amount: u64,
        to: Pubkey,
        memo: String,
    ) -> GatewayResult<Signature> {
        // Create recipient token account, if necessary
        self.create_token_account_ore(to).await?;

        // Submit transfer ix
        let signer = signer();
        let from_token_account = ore_token_account_address(signer.pubkey());
        let to_token_account = ore_token_account_address(to);
        let memo_ix = spl_memo::build_memo(&memo.into_bytes(), &[&signer.pubkey()]);
        let transfer_ix = spl_token::instruction::transfer(
            &spl_token::ID,
            &from_token_account,
            &to_token_account,
            &signer.pubkey(),
            &[&signer.pubkey()],
            amount,
        )
        .unwrap();
        self.send_and_confirm(&[memo_ix, transfer_ix], true, false)
            .await
    }

    pub async fn create_token_account_ore(&self, owner: Pubkey) -> GatewayResult<Pubkey> {
        // Build instructions.
        let signer = signer();

        // Check if account already exists.
        let token_account_address = ore_token_account_address(owner);
        match self
            .rpc
            .get_token_account(&token_account_address)
            .await
            .map_err(GatewayError::from)
        {
            Ok(token_account) => {
                if token_account.is_some() {
                    return Ok(token_account_address);
                }
            }
            Err(err) => {
                log::info!("Err: {:?}", err);
                match err {
                    GatewayError::AccountNotFound => {}
                    _ => return Err(err),
                }
            }
        }

        // Sign and send transaction.
        let ix = create_associated_token_account(
            &signer.pubkey(),
            &owner,
            &ore::MINT_ADDRESS,
            &spl_token::id(),
        );
        match self.send_and_confirm(&[ix], true, false).await {
            Ok(_) => track(AppEvent::CreateTokenAccount, None),
            Err(_) => return Err(GatewayError::FailedAta),
        }

        // Return token account address
        Ok(token_account_address)
    }

    // API
    pub async fn get_transfer(&self, sig: String) -> Option<Transfer> {
        let client = reqwest::Client::new();
        match client
            .get(format!("{}/transfers/{}", self.api_url, sig))
            .send()
            .await
        {
            Ok(res) => res.json::<Transfer>().await.ok(),
            Err(e) => {
                log::error!("{:?}", e);
                None
            }
        }
    }

    pub async fn list_transfers(
        &self,
        user: Option<Pubkey>,
        offset: u64,
        limit: usize,
    ) -> Option<GetTransfersResponse> {
        let offset = offset.to_string();
        let limit = limit.to_string();
        let mut query = vec![("offset", offset.as_str()), ("limit", limit.as_str())];
        let user_str = user.map(|u| u.to_string());
        let user_ref = user_str.as_deref();
        if let Some(user_str) = user_ref {
            query.push(("user", user_str));
        };
        let client = reqwest::Client::new();
        match client
            .get(format!("{}/transfers", &self.api_url))
            .query(&query)
            .send()
            .await
        {
            Ok(res) => res.json::<GetTransfersResponse>().await.ok(),
            Err(e) => {
                log::error!("{:?}", e);
                None
            }
        }
    }
}

#[cfg(feature = "web")]
pub fn signer() -> Keypair {
    let key = "keypair";
    let value = LocalStorage::get(key).ok().unwrap_or_else(|| {
        let x = Keypair::new().to_base58_string();
        LocalStorage::set(key, &x).ok();
        x
    });
    Keypair::from_base58_string(&value)
}

#[cfg(feature = "desktop")]
pub fn signer() -> Keypair {
    use crate::file::{get_value, set_key_value};

    let key = "keypair";
    let value = get_value(key).ok().unwrap_or_else(|| {
        let value = Keypair::new().to_base58_string();
        if let Ok(v) = serde_json::to_value(&value) {
            set_key_value(key, &v).ok();
        }
        value
    });
    Keypair::from_base58_string(&value)
}

#[cached]
pub fn ore_token_account_address(pubkey: Pubkey) -> Pubkey {
    get_associated_token_address(&pubkey, &ore::MINT_ADDRESS)
}
