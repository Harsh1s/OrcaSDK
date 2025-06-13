use std::{collections::HashSet, str::FromStr, time::Duration};

use anyhow::{anyhow, Result};
use bs58::decode;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::{
        instruction::{create_lookup_table, extend_lookup_table},
        state::AddressLookupTable,
        AddressLookupTableAccount,
    },
    commitment_config::CommitmentLevel,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    signer::SeedDerivable,
    transaction::{Transaction, VersionedTransaction},
};
use tokio::time::sleep;

use orca_tx_sender::{get_rpc_client, set_rpc};
use orca_whirlpools::{
    set_whirlpools_config_address, swap_instructions, SwapType, WhirlpoolsConfigInput,
};

fn keypair_from_base58_seed(secret_b58: &str) -> Result<Keypair> {
    let seed_vec = decode(secret_b58)
        .into_vec()
        .map_err(|e| anyhow!("base58 decode error: {}", e))?;
    if seed_vec.len() != 32 {
        return Err(anyhow!("expected 32-byte seed, got {}", seed_vec.len()));
    }
    let seed: [u8; 32] = seed_vec.try_into().unwrap();
    Keypair::from_seed(&seed).map_err(|e| anyhow!("Failed to create keypair from seed: {}", e))
}

async fn fetch_address_lookup_table(
    rpc: &RpcClient,
    alt_address: &Pubkey,
) -> Result<AddressLookupTableAccount> {
    let raw = rpc
        .get_account_with_commitment(
            alt_address,
            orca_tx_sender::CommitmentConfig {
                commitment: CommitmentLevel::Confirmed,
            },
        )
        .await?
        .value
        .ok_or_else(|| anyhow!("ALT account {} not found", alt_address))?;

    let table = AddressLookupTable::deserialize(&raw.data)
        .map_err(|e| anyhow!("Failed to deserialize ALT {}: {}", alt_address, e))?;
    println!(
        "Fetched ALT {} with {} addresses.",
        alt_address,
        table.addresses.len()
    );
    Ok(AddressLookupTableAccount {
        key: *alt_address,
        addresses: table.addresses.to_vec(),
    })
}

async fn create_v0_tx_with_lookup(
    rpc: &RpcClient,
    payer: &Keypair,
    ixns: Vec<Instruction>,
    lookup_table_key: Pubkey,
    extra_signers: &[&Keypair],
) -> Result<VersionedTransaction> {
    println!("Fetching ALT {}...", lookup_table_key);
    let mut alt_account = fetch_address_lookup_table(rpc, &lookup_table_key).await?;
    // Wait and fetch again with higher commitment to ensure finalized
    sleep(Duration::from_secs(5)).await;
    alt_account = fetch_address_lookup_table(rpc, &lookup_table_key).await?;

    // After fetching the ALT
    println!("ALT contains {} addresses:", alt_account.addresses.len());
    for (i, addr) in alt_account.addresses.iter().enumerate() {
        println!("  [{}]: {}", i, addr);
    }

    println!("Fetching recent blockhash...");
    let blockhash = rpc
        .get_latest_blockhash_with_commitment(orca_tx_sender::CommitmentConfig {
            commitment: CommitmentLevel::Confirmed,
        })
        .await?
        .0;

    println!("Compiling V0 message...");
    let message = VersionedMessage::V0(v0::Message::try_compile(
        &payer.pubkey(),
        &ixns,
        &[alt_account.clone()],
        blockhash,
    )?);
    println!("Message compiled successfully.");

    // --- Insert account presence check here ---
    // Collect all accounts used in the instructions
    let all_accounts: Vec<Pubkey> = ixns
        .iter()
        .flat_map(|ix| ix.accounts.iter().map(|am| am.pubkey))
        .collect();

    if let VersionedMessage::V0(ref v0_msg) = message {
        for account in &all_accounts {
            if !alt_account.addresses.contains(account) && !v0_msg.account_keys.contains(account) {
                println!(
                    "WARNING: Account {} not found in ALT or static accounts",
                    account
                );
            }
        }
    }
    // --- End account presence check ---

    // Create a deduplicated signers list
    let mut signers_set = HashSet::new();
    let mut signers_to_provide: Vec<&Keypair> = Vec::new();

    // Add payer first (always required)
    signers_set.insert(payer.pubkey());
    signers_to_provide.push(payer);

    // Add the rest of signers, avoiding duplicates
    for signer in extra_signers {
        if signers_set.insert(signer.pubkey()) {
            signers_to_provide.push(signer);
        } else {
            println!("INFO: Skipping duplicate signer: {}", signer.pubkey());
        }
    }

    if let VersionedMessage::V0(ref v0_msg) = message {
        println!("\n--- Signer Debugging ---");
        println!("Compiled V0 Message Header: {:?}", v0_msg.header);

        let num_required = v0_msg.header.num_required_signatures as usize;
        let static_keys = &v0_msg.account_keys;

        if num_required > static_keys.len() {
            println!(
                "Error: Message header indicates more signers ({}) than static keys ({})!",
                num_required,
                static_keys.len()
            );
            return Err(anyhow!(
                "Internal Error: Compiled message has inconsistent signer count."
            ));
        } else {
            let required_signer_pubkeys = &static_keys[..num_required];
            println!(
                "Required Signer Pubkeys (determined by try_compile): {:?}",
                required_signer_pubkeys
            );

            println!("Required signers: {:?}", required_signer_pubkeys);
            for (i, pk) in required_signer_pubkeys.iter().enumerate() {
                println!("Signer {}: {}", i, pk);
            }

            let provided_signer_pubkeys: Vec<Pubkey> =
                signers_to_provide.iter().map(|s| s.pubkey()).collect();
            println!(
                "Provided Signer Pubkeys (for try_new): {:?}",
                provided_signer_pubkeys
            );

            let mut missing = false;
            for req_signer in required_signer_pubkeys {
                if !provided_signer_pubkeys.contains(req_signer) {
                    println!(
                        "ERROR: Missing required signer in provided list: {}",
                        req_signer
                    );
                    missing = true;
                }
            }

            for prov_signer_key in &provided_signer_pubkeys {
                if !required_signer_pubkeys.contains(prov_signer_key) {
                    println!(
                         "WARNING: Provided signer {} is NOT in the required list determined by try_compile.",
                         prov_signer_key
                     );
                }
            }

            if missing {
                return Err(anyhow!(
                    "Debug Check Failed: Not all required signers were provided."
                ));
            } else {
                println!("Debug Check Passed: All required signers appear to be provided.");
            }
        }
        println!("--- End Signer Debugging ---\n");
    }

    println!("Attempting to create signed VersionedTransaction...");
    VersionedTransaction::try_new(message, &signers_to_provide)
        .map_err(|e| anyhow!("Failed to create VersionedTransaction (try_new): {}", e))
}

#[tokio::main]
async fn main() -> Result<()> {
    let secret = "Eh8SPgsRqUp8Nj2WckKqzLxv1Cjr3KUKxshgEeCkFZYS";
    let wallet = keypair_from_base58_seed(secret)?;
    println!("Imported Wallet Pubkey = {}", wallet.pubkey());

    let rpc_url = "https://api.devnet.solana.com";
    println!("Setting RPC to: {}", rpc_url);
    set_rpc(rpc_url).await.map_err(|e| anyhow!(e))?;
    println!("Setting Whirlpools Config for Devnet...");
    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaDevnet)
        .map_err(|e| anyhow!("Failed to set Whirlpools config: {}", e))?;
    let rpc = get_rpc_client().map_err(|e| anyhow!("Failed to get RPC client: {}", e))?;
    println!("RPC Client and Whirlpools Config set.");

    let min_balance = 100_000_000u64;
    let current_bal = rpc.get_balance(&wallet.pubkey()).await?;
    println!("Wallet balance: {} lamports", current_bal);
    if current_bal < min_balance {
        println!(
            "Balance {} is less than minimum {}. Requesting airdrop...",
            current_bal, min_balance
        );
        let airdrop_amount = 1_000_000_000u64;
        let sig = rpc
            .request_airdrop(&wallet.pubkey(), airdrop_amount)
            .await?;
        println!("Airdrop requested. Signature: {}", sig);
        println!("Waiting for airdrop confirmation...");
        rpc.confirm_transaction_with_commitment(
            &sig,
            orca_tx_sender::CommitmentConfig {
                commitment: CommitmentLevel::Confirmed,
            },
        )
        .await?;
        println!("Airdrop confirmed. Please allow a few seconds for balance update.");
        sleep(Duration::from_secs(10)).await;
        let new_bal = rpc.get_balance(&wallet.pubkey()).await?;
        println!("New wallet balance: {} lamports", new_bal);
        if new_bal < min_balance {
            println!("WARN: Balance still low after airdrop. Continuing anyway.");
        }
    }

    let transfer_authority = Keypair::new();
    println!(
        "Generated Transfer Authority Pubkey: {}",
        transfer_authority.pubkey()
    );

    let source_token_account = Keypair::new();
    println!(
        "Generated Source Token Account Pubkey: {}",
        source_token_account.pubkey()
    );

    let whirlpool_address_str = "3KBZiL2g8C7tiJ32hTv5v3KM7aK9htpqTw4cTXz1HvPt";
    let mint_in_str = "So11111111111111111111111111111111111111112";
    let whirlpool = Pubkey::from_str(whirlpool_address_str)?;
    let mint_in = Pubkey::from_str(mint_in_str)?;
    let input_amount = 1_000_000u64;
    let slippage = Some(50u16);

    println!(
        "Building swap instructions for {} -> ? in pool {}...",
        mint_in_str, whirlpool_address_str
    );
    let swap_res = swap_instructions(
        &rpc,
        whirlpool,
        input_amount,
        mint_in,
        SwapType::ExactIn,
        slippage,
        Some(transfer_authority.pubkey()),
    )
    .await
    .map_err(|e| anyhow!("Failed to get swap instructions: {}", e))?;
    println!(
        "Swap instructions generated. Quote out ≈ {:?}. Instructions count: {}",
        swap_res.quote,
        swap_res.instructions.len()
    );

    for (i, ix) in swap_res.instructions.iter().enumerate() {
        println!("Instruction {}: {:?}", i, ix.accounts);
    }

    // Create a deduplicated signers list
    let mut unique_signers = HashSet::new();
    let mut all_signers = Vec::new();

    // Add required signers first
    unique_signers.insert(wallet.pubkey());
    all_signers.push(&wallet);

    unique_signers.insert(transfer_authority.pubkey());
    all_signers.push(&transfer_authority);

    // Add any additional signers without duplicates
    for signer in swap_res.additional_signers.iter() {
        if unique_signers.insert(signer.pubkey()) {
            all_signers.push(signer);
        } else {
            println!("Skipping duplicate signer: {}", signer.pubkey());
        }
    }

    println!("Creating Address Lookup Table (ALT)...");
    let creation_slot = rpc
        .get_slot_with_commitment(orca_tx_sender::CommitmentConfig {
            commitment: CommitmentLevel::Confirmed,
        })
        .await?;
    let (create_ix, lut_key) = create_lookup_table(wallet.pubkey(), wallet.pubkey(), creation_slot);

    let create_bh = rpc
        .get_latest_blockhash_with_commitment(orca_tx_sender::CommitmentConfig {
            commitment: CommitmentLevel::Confirmed,
        })
        .await?
        .0;
    let create_tx = Transaction::new_signed_with_payer(
        &[create_ix],
        Some(&wallet.pubkey()),
        &[&wallet],
        create_bh,
    );

    println!("Sending ALT creation transaction...");
    let create_sig = rpc
        .send_and_confirm_transaction_with_spinner_and_commitment(
            &create_tx,
            orca_tx_sender::CommitmentConfig {
                commitment: CommitmentLevel::Confirmed,
            },
        )
        .await?;
    println!("ALT creation confirmed. Signature: {}", create_sig);
    println!("Created ALT: {}", lut_key);
    sleep(Duration::from_secs(5)).await;

    println!("Gathering accounts needed for the swap...");
    let mut all_accounts = swap_res
        .instructions
        .iter()
        .flat_map(|ix| ix.accounts.iter().map(|am| am.pubkey))
        .collect::<Vec<_>>();
    all_accounts.push(wallet.pubkey());
    all_accounts.push(transfer_authority.pubkey());
    all_accounts.push(source_token_account.pubkey());

    all_accounts.sort_unstable();
    all_accounts.dedup();
    println!(
        "Extending ALT {} with {} unique addresses...",
        lut_key,
        all_accounts.len()
    );

    for chunk in all_accounts.chunks(20) {
        println!("Extending with chunk of {} addresses...", chunk.len());
        let extend_ix = extend_lookup_table(
            lut_key,
            wallet.pubkey(),
            Some(wallet.pubkey()),
            chunk.to_vec(),
        );
        let extend_bh = rpc
            .get_latest_blockhash_with_commitment(orca_tx_sender::CommitmentConfig {
                commitment: CommitmentLevel::Confirmed,
            })
            .await?
            .0;
        let extend_tx = Transaction::new_signed_with_payer(
            &[extend_ix],
            Some(&wallet.pubkey()),
            &[&wallet],
            extend_bh,
        );

        println!("Sending ALT extension transaction...");
        let extend_sig = rpc
            .send_and_confirm_transaction_with_spinner_and_commitment(
                &extend_tx,
                orca_tx_sender::CommitmentConfig {
                    commitment: CommitmentLevel::Confirmed,
                },
            )
            .await?;
        println!("ALT extension chunk confirmed. Signature: {}", extend_sig);
        println!("Waiting for ALT update to finalize...");
        sleep(Duration::from_secs(10)).await; // Increased from 2 seconds
    }
    println!("Finished extending ALT {}.", lut_key);
    sleep(Duration::from_secs(5)).await;

    println!(
        "Creating the final V0 swap transaction using ALT {}...",
        lut_key
    );
    let v0_swap_tx =
        create_v0_tx_with_lookup(&rpc, &wallet, swap_res.instructions, lut_key, &all_signers)
            .await?;

    // Before sending the transaction
    if let VersionedMessage::V0(ref v0_msg) = v0_swap_tx.message {
        println!("Validating V0 transaction lookups...");

        // Fetch the ALT again to validate against the transaction
        let alt_account = fetch_address_lookup_table(&rpc, &lut_key).await?;

        // Check lookup table contents vs transaction needs
        for (table_idx, table) in v0_msg.address_table_lookups.iter().enumerate() {
            if table.account_key != lut_key {
                println!("WARNING: Transaction uses unexpected ALT: {}", table.account_key);
            }

            let max_readonly_idx = table.readonly_indexes.iter().max().unwrap_or(&0);
            let max_writable_idx = table.writable_indexes.iter().max().unwrap_or(&0);
            let max_idx = std::cmp::max(*max_readonly_idx, *max_writable_idx);

            if max_idx as usize >= alt_account.addresses.len() {
                println!(
                    "ERROR: Transaction wants to access index {} but ALT only has {} addresses",
                    max_idx,
                    alt_account.addresses.len()
                );
                return Err(anyhow!("Invalid ALT index in transaction"));
            }
        }
    }

    println!("Sending V0 swap transaction...");
    let swap_sig = rpc.send_transaction(&v0_swap_tx).await?;

    println!("Waiting for swap confirmation...");
    rpc.confirm_transaction_with_commitment(
        &swap_sig,
        orca_tx_sender::CommitmentConfig {
            commitment: CommitmentLevel::Confirmed,
        },
    )
    .await?;
    println!("Swap confirmed successfully! Signature: {}", swap_sig);
    println!(
        "Check transaction details on Solana Explorer: https://explorer.solana.com/tx/{}?cluster=devnet",
        swap_sig
    );

    Ok(())
}
