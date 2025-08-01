use std::sync::Arc;

use anyhow::{bail, Result};
use bip39::Mnemonic;
use cashu::amount::SplitTarget;
use cashu::nut23::Amountless;
use cashu::{Amount, CurrencyUnit, MintRequest, PreMintSecrets, ProofsMethods};
use cdk::wallet::{HttpClient, MintConnector, Wallet};
use cdk_integration_tests::init_regtest::get_cln_dir;
use cdk_integration_tests::{get_mint_url_from_env, wait_for_mint_to_be_paid};
use cdk_sqlite::wallet::memory;
use ln_regtest_rs::ln_client::ClnClient;

/// Tests basic BOLT12 minting functionality:
/// - Creates a wallet
/// - Gets a BOLT12 quote for a specific amount (100 sats)
/// - Pays the quote using Core Lightning
/// - Mints tokens and verifies the correct amount is received
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_regtest_bolt12_mint() {
    let wallet = Wallet::new(
        &get_mint_url_from_env(),
        CurrencyUnit::Sat,
        Arc::new(memory::empty().await.unwrap()),
        &Mnemonic::generate(12).unwrap().to_seed_normalized(""),
        None,
    )
    .unwrap();

    let mint_amount = Amount::from(100);

    let mint_quote = wallet
        .mint_bolt12_quote(Some(mint_amount), None)
        .await
        .unwrap();

    assert_eq!(mint_quote.amount, Some(mint_amount));

    let cln_one_dir = get_cln_dir("one");
    let cln_client = ClnClient::new(cln_one_dir.clone(), None).await.unwrap();
    cln_client
        .pay_bolt12_offer(None, mint_quote.request)
        .await
        .unwrap();

    let proofs = wallet
        .mint_bolt12(&mint_quote.id, None, SplitTarget::default(), None)
        .await
        .unwrap();

    assert_eq!(proofs.total_amount().unwrap(), 100.into());
}

/// Tests multiple payments to a single BOLT12 quote:
/// - Creates a wallet and gets a BOLT12 quote without specifying amount
/// - Makes two separate payments (10,000 sats and 11,000 sats) to the same quote
/// - Verifies that each payment can be minted separately and correctly
/// - Tests the functionality of reusing a quote for multiple payments
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_regtest_bolt12_mint_multiple() -> Result<()> {
    let wallet = Wallet::new(
        &get_mint_url_from_env(),
        CurrencyUnit::Sat,
        Arc::new(memory::empty().await?),
        &Mnemonic::generate(12)?.to_seed_normalized(""),
        None,
    )?;

    let mint_quote = wallet.mint_bolt12_quote(None, None).await?;

    let cln_one_dir = get_cln_dir("one");
    let cln_client = ClnClient::new(cln_one_dir.clone(), None).await?;
    cln_client
        .pay_bolt12_offer(Some(10000), mint_quote.request.clone())
        .await
        .unwrap();

    wait_for_mint_to_be_paid(&wallet, &mint_quote.id, 60).await?;

    wallet.mint_bolt12_quote_state(&mint_quote.id).await?;

    let proofs = wallet
        .mint_bolt12(&mint_quote.id, None, SplitTarget::default(), None)
        .await
        .unwrap();

    assert_eq!(proofs.total_amount().unwrap(), 10.into());

    cln_client
        .pay_bolt12_offer(Some(11_000), mint_quote.request)
        .await
        .unwrap();

    wait_for_mint_to_be_paid(&wallet, &mint_quote.id, 60).await?;

    wallet.mint_bolt12_quote_state(&mint_quote.id).await?;

    let proofs = wallet
        .mint_bolt12(&mint_quote.id, None, SplitTarget::default(), None)
        .await
        .unwrap();

    assert_eq!(proofs.total_amount().unwrap(), 11.into());

    Ok(())
}

/// Tests that multiple wallets can pay the same BOLT12 offer:
/// - Creates a BOLT12 offer through CLN that both wallets will pay
/// - Creates two separate wallets with different minting amounts
/// - Has each wallet get their own quote and make payments
/// - Verifies both wallets can successfully mint their tokens
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_regtest_bolt12_multiple_wallets() -> Result<()> {
    // Create first wallet
    let wallet_one = Wallet::new(
        &get_mint_url_from_env(),
        CurrencyUnit::Sat,
        Arc::new(memory::empty().await?),
        &Mnemonic::generate(12)?.to_seed_normalized(""),
        None,
    )?;

    // Create second wallet
    let wallet_two = Wallet::new(
        &get_mint_url_from_env(),
        CurrencyUnit::Sat,
        Arc::new(memory::empty().await?),
        &Mnemonic::generate(12)?.to_seed_normalized(""),
        None,
    )?;

    // Create a BOLT12 offer that both wallets will use
    let cln_one_dir = get_cln_dir("one");
    let cln_client = ClnClient::new(cln_one_dir.clone(), None).await?;
    // First wallet payment
    let quote_one = wallet_one
        .mint_bolt12_quote(Some(10_000.into()), None)
        .await?;
    cln_client
        .pay_bolt12_offer(None, quote_one.request.clone())
        .await?;
    wait_for_mint_to_be_paid(&wallet_one, &quote_one.id, 60).await?;
    let proofs_one = wallet_one
        .mint_bolt12(&quote_one.id, None, SplitTarget::default(), None)
        .await?;

    assert_eq!(proofs_one.total_amount()?, 10_000.into());

    // Second wallet payment
    let quote_two = wallet_two
        .mint_bolt12_quote(Some(15_000.into()), None)
        .await?;
    cln_client
        .pay_bolt12_offer(None, quote_two.request.clone())
        .await?;
    wait_for_mint_to_be_paid(&wallet_two, &quote_two.id, 60).await?;

    let proofs_two = wallet_two
        .mint_bolt12(&quote_two.id, None, SplitTarget::default(), None)
        .await?;
    assert_eq!(proofs_two.total_amount()?, 15_000.into());

    let offer = cln_client
        .get_bolt12_offer(None, false, "test_multiple_wallets".to_string())
        .await?;

    let wallet_one_melt_quote = wallet_one
        .melt_bolt12_quote(
            offer.to_string(),
            Some(cashu::MeltOptions::Amountless {
                amountless: Amountless {
                    amount_msat: 1500.into(),
                },
            }),
        )
        .await?;

    let wallet_two_melt_quote = wallet_two
        .melt_bolt12_quote(
            offer.to_string(),
            Some(cashu::MeltOptions::Amountless {
                amountless: Amountless {
                    amount_msat: 1000.into(),
                },
            }),
        )
        .await?;

    let melted = wallet_one.melt(&wallet_one_melt_quote.id).await?;

    assert!(melted.preimage.is_some());

    let melted_two = wallet_two.melt(&wallet_two_melt_quote.id).await?;

    assert!(melted_two.preimage.is_some());

    Ok(())
}

/// Tests the BOLT12 melting (spending) functionality:
/// - Creates a wallet and mints 20,000 sats using BOLT12
/// - Creates a BOLT12 offer for 10,000 sats
/// - Tests melting (spending) tokens using the BOLT12 offer
/// - Verifies the correct amount is melted
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_regtest_bolt12_melt() -> Result<()> {
    let wallet = Wallet::new(
        &get_mint_url_from_env(),
        CurrencyUnit::Sat,
        Arc::new(memory::empty().await?),
        &Mnemonic::generate(12)?.to_seed_normalized(""),
        None,
    )?;

    wallet.get_mint_info().await?;

    let mint_amount = Amount::from(20_000);

    // Create a single-use BOLT12 quote
    let mint_quote = wallet.mint_bolt12_quote(Some(mint_amount), None).await?;

    assert_eq!(mint_quote.amount, Some(mint_amount));
    // Pay the quote
    let cln_one_dir = get_cln_dir("one");
    let cln_client = ClnClient::new(cln_one_dir.clone(), None).await?;
    cln_client
        .pay_bolt12_offer(None, mint_quote.request.clone())
        .await?;

    // Wait for payment to be processed
    wait_for_mint_to_be_paid(&wallet, &mint_quote.id, 60).await?;

    let offer = cln_client
        .get_bolt12_offer(Some(10_000), true, "hhhhhhhh".to_string())
        .await?;

    let _proofs = wallet
        .mint_bolt12(&mint_quote.id, None, SplitTarget::default(), None)
        .await
        .unwrap();

    let quote = wallet.melt_bolt12_quote(offer.to_string(), None).await?;

    let melt = wallet.melt(&quote.id).await?;

    assert_eq!(melt.amount, 10.into());

    Ok(())
}

/// Tests security validation for BOLT12 minting to prevent overspending:
/// - Creates a wallet and gets an open-ended BOLT12 quote
/// - Makes a payment of 10,000 millisats
/// - Attempts to mint more tokens (500 sats) than were actually paid for
/// - Verifies that the mint correctly rejects the oversized mint request
/// - Ensures proper error handling with TransactionUnbalanced error
/// This test is crucial for ensuring the economic security of the minting process
/// by preventing users from minting more tokens than they have paid for.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_regtest_bolt12_mint_extra() -> Result<()> {
    let wallet = Wallet::new(
        &get_mint_url_from_env(),
        CurrencyUnit::Sat,
        Arc::new(memory::empty().await?),
        &Mnemonic::generate(12)?.to_seed_normalized(""),
        None,
    )?;

    wallet.get_mint_info().await?;

    // Create a single-use BOLT12 quote
    let mint_quote = wallet.mint_bolt12_quote(None, None).await?;

    let state = wallet.mint_bolt12_quote_state(&mint_quote.id).await?;

    assert_eq!(state.amount_paid, Amount::ZERO);
    assert_eq!(state.amount_issued, Amount::ZERO);

    let active_keyset_id = wallet.fetch_active_keyset().await?.id;

    let pay_amount_msats = 10_000;

    let cln_one_dir = get_cln_dir("one");
    let cln_client = ClnClient::new(cln_one_dir.clone(), None).await?;
    cln_client
        .pay_bolt12_offer(Some(pay_amount_msats), mint_quote.request.clone())
        .await?;

    wait_for_mint_to_be_paid(&wallet, &mint_quote.id, 10).await?;

    let state = wallet.mint_bolt12_quote_state(&mint_quote.id).await?;

    assert_eq!(state.amount_paid, (pay_amount_msats / 1_000).into());
    assert_eq!(state.amount_issued, Amount::ZERO);

    let pre_mint = PreMintSecrets::random(active_keyset_id, 500.into(), &SplitTarget::None)?;

    let quote_info = wallet
        .localstore
        .get_mint_quote(&mint_quote.id)
        .await?
        .expect("there is a quote");

    let mut mint_request = MintRequest {
        quote: mint_quote.id,
        outputs: pre_mint.blinded_messages(),
        signature: None,
    };

    if let Some(secret_key) = quote_info.secret_key {
        mint_request.sign(secret_key)?;
    }

    let http_client = HttpClient::new(get_mint_url_from_env().parse().unwrap(), None);

    let response = http_client.post_mint(mint_request.clone()).await;

    match response {
        Err(err) => match err {
            cdk::Error::TransactionUnbalanced(_, _, _) => (),
            err => {
                bail!("Wrong mint error returned: {}", err.to_string());
            }
        },
        Ok(_) => {
            bail!("Should not have allowed second payment");
        }
    }

    Ok(())
}
