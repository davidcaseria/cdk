//! Wallet based on Nostr

use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::Arc,
    time::Duration,
    vec,
};

use async_trait::async_trait;
use itertools::Itertools;
use nostr_database::DatabaseError;
use nostr_sdk::{
    client, nips::nip44, Client, Event, EventBuilder, EventId, Filter, FilterOptions, Kind,
    NostrDatabase, RelayMessage, RelayPoolNotification, SecretKey, SingleLetterTag,
    SubscribeAutoCloseOptions, Tag, TagKind,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use url::Url;

use crate::{
    nuts::{
        CurrencyUnit, Id, KeySetInfo, Keys, MeltQuoteState, MintInfo, MintQuoteState, Proof,
        PublicKey, SpendingConditions, State,
    },
    types::ProofInfo,
    wallet::{MeltQuote, MintQuote},
    Amount, UncheckedUrl,
};

use super::WalletDatabase;

const TX_KIND: Kind = Kind::Custom(7375);
const QUOTE_KIND: Kind = Kind::Custom(7376);
const WALLET_INFO_KIND: Kind = Kind::Custom(37375);
// const SNAPSHOT_KIND: Kind = Kind::Custom(37376);
const ID_TAG: char = 'd';
const ID_LINK_TAG: char = 'a';
const MINT_TAG: &str = "mint";
const NAME_TAG: &str = "name";
const UNIT_TAG: &str = "unit";
const DESCRIPTION_TAG: &str = "description";
const RELAY_TAG: &str = "relay";
const BALANCE_TAG: &str = "balance";
const PRIVKEY_TAG: &str = "privkey";
const COUNTER_TAG: &str = "counter";
const QUOTE_ID_TAG: &str = "quote_id";
const AMOUNT_TAG: &str = "amount";
const REQUEST_TAG: &str = "request";
const STATE_TAG: &str = "state";
const FEE_RESERVE: &str = "fee_reserve";
const PREIMAGE_TAG: &str = "preimage";
const EXPIRATION_TAG: &str = "expiration";

/// Wallet on Nostr
#[derive(Clone, Debug)]
pub struct WalletNostrDatabase {
    client: Client,
    keys: nostr_sdk::Keys,
    id: String,

    // Local disk storage
    db: Option<Arc<Box<dyn NostrDatabase<Err = DatabaseError>>>>,

    // In-memory storage
    mint_keysets: Arc<RwLock<HashMap<UncheckedUrl, HashSet<Id>>>>,
    keysets: Arc<RwLock<HashMap<Id, KeySetInfo>>>,
    mint_keys: Arc<RwLock<HashMap<Id, Keys>>>,
    proof_states: Arc<RwLock<HashMap<PublicKey, State>>>,
}

impl WalletNostrDatabase {
    /// Create a new WalletNostrDatabase
    pub async fn remote(
        id: String,
        keys: nostr_sdk::Keys,
        relays: Vec<Url>,
    ) -> Result<Self, Error> {
        let client = Client::new(&keys);
        client.add_relays(relays).await?;
        client.connect().await;
        Ok(Self::new(client, keys, id, None))
    }

    /// Create a new WalletNostrDatabase with a local database
    pub async fn local<D>(
        id: String,
        keys: nostr_sdk::Keys,
        relays: Vec<Url>,
        db: D,
    ) -> Result<Self, Error>
    where
        D: NostrDatabase<Err = DatabaseError> + 'static,
    {
        let client = Client::new(&keys);
        client.add_relays(relays).await?;
        client.connect().await;
        Ok(Self::new(client, keys, id, Some(Arc::new(Box::new(db)))))
    }

    fn new(
        client: Client,
        keys: nostr_sdk::Keys,
        id: String,
        db: Option<Arc<Box<dyn NostrDatabase<Err = DatabaseError>>>>,
    ) -> Self {
        Self {
            client,
            keys,
            id,
            db,
            mint_keysets: Arc::new(RwLock::new(HashMap::new())),
            keysets: Arc::new(RwLock::new(HashMap::new())),
            mint_keys: Arc::new(RwLock::new(HashMap::new())),
            proof_states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get wallet info
    pub async fn get_wallet_info(&self) -> Result<WalletInfo, Error> {
        let filters = vec![Filter {
            kinds: Some(vec![WALLET_INFO_KIND].into_iter().collect()),
            generic_tags: vec![(
                SingleLetterTag::from_char(ID_TAG).expect("ID_TAG is not a single letter tag"),
                vec![self.id.clone()].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        }];
        let events = self.get_events(filters).await?;
        match latest_event(events) {
            Some(event) => WalletInfo::from_event(&event, &self.keys),
            None => {
                let info = WalletInfo {
                    id: self.id.clone(),
                    balance: None,
                    mints: HashSet::new(),
                    name: None,
                    unit: None,
                    description: None,
                    relays: HashSet::new(),
                    p2pk_priv_key: None,
                    counters: HashMap::new(),
                };
                self.save_wallet_info(info.clone()).await?;
                Ok(info)
            }
        }
    }

    /// Save wallet info
    pub async fn save_wallet_info(&self, info: WalletInfo) -> Result<EventId, Error> {
        let event = info.to_event(&self.keys)?;
        Ok(self.client.send_event(event).await?.val)
    }

    async fn get_events(&self, filters: Vec<Filter>) -> Result<Vec<Event>, Error> {
        if let Some(db) = &self.db {
            Ok(db.query(filters, nostr_database::Order::Asc).await?)
        } else {
            let mut notifications = self.client.notifications();
            let sub_id = self
                .client
                .subscribe(
                    filters,
                    Some(
                        SubscribeAutoCloseOptions::default()
                            .filter(FilterOptions::ExitOnEOSE)
                            .timeout(Some(Duration::from_secs(10))),
                    ),
                )
                .await?
                .val;
            let mut events = Vec::new();
            let mut relay_urls: HashSet<Url> = self.client.relays().await.keys().cloned().collect();
            loop {
                match notifications.recv().await {
                    Ok(RelayPoolNotification::Event {
                        subscription_id,
                        event,
                        ..
                    }) => {
                        if subscription_id == sub_id {
                            events.push(*event);
                        }
                    }
                    Ok(RelayPoolNotification::Message { relay_url, message }) => match message {
                        RelayMessage::EndOfStoredEvents(_) => {
                            relay_urls.remove(&relay_url);
                            if relay_urls.is_empty() {
                                break;
                            }
                        }
                        _ => {}
                    },
                    Ok(_) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(events)
        }
    }
}

fn latest_event(events: Vec<Event>) -> Option<Event> {
    let mut latest: Option<Event> = None;
    for event in events {
        if let Some(latest_event) = latest.as_ref() {
            if latest_event.created_at() < event.created_at() {
                latest = Some(event);
            }
        } else {
            latest = Some(event);
        }
    }
    latest
}

fn unique_events(events: Vec<Event>) -> Vec<Event> {
    let mut unique = Vec::new();
    let mut ids = HashSet::new();
    for event in events {
        if !ids.contains(&event.id()) {
            ids.insert(event.id());
            unique.push(event);
        }
    }
    unique
}

#[async_trait]
impl WalletDatabase for WalletNostrDatabase {
    type Err = super::Error;

    async fn add_mint(
        &self,
        mint_url: UncheckedUrl,
        _mint_info: Option<MintInfo>,
    ) -> Result<(), Self::Err> {
        let mut info = self.get_wallet_info().await.map_err(map_err)?;
        info.mints.insert(mint_url);
        self.save_wallet_info(info).await.map_err(map_err)?;
        Ok(())
    }

    async fn remove_mint(&self, mint_url: UncheckedUrl) -> Result<(), Self::Err> {
        let mut info = self.get_wallet_info().await.map_err(map_err)?;
        info.mints.retain(|url| url != &mint_url);
        self.save_wallet_info(info).await.map_err(map_err)?;
        Ok(())
    }

    async fn get_mint(&self, _mint_url: UncheckedUrl) -> Result<Option<MintInfo>, Self::Err> {
        Ok(None)
    }

    async fn get_mints(&self) -> Result<HashMap<UncheckedUrl, Option<MintInfo>>, Self::Err> {
        let info = self.get_wallet_info().await.map_err(map_err)?;
        let mut mints = HashMap::new();
        for mint in info.mints {
            mints.insert(mint, None);
        }
        Ok(mints)
    }

    async fn update_mint_url(
        &self,
        old_mint_url: UncheckedUrl,
        new_mint_url: UncheckedUrl,
    ) -> Result<(), Self::Err> {
        let mut info = self.get_wallet_info().await.map_err(map_err)?;
        info.mints.retain(|url| url != &old_mint_url);
        info.mints.insert(new_mint_url);
        self.save_wallet_info(info).await.map_err(map_err)?;
        Ok(())
    }

    async fn add_mint_keysets(
        &self,
        mint_url: UncheckedUrl,
        keysets: Vec<KeySetInfo>,
    ) -> Result<(), Self::Err> {
        let mut current_mint_keysets = self.mint_keysets.write().await;
        let mut current_keysets = self.keysets.write().await;

        for keyset in keysets {
            current_mint_keysets
                .entry(mint_url.clone())
                .and_modify(|ks| {
                    ks.insert(keyset.id);
                })
                .or_insert(HashSet::from_iter(vec![keyset.id]));

            current_keysets.insert(keyset.id, keyset);
        }

        Ok(())
    }

    async fn get_mint_keysets(
        &self,
        mint_url: UncheckedUrl,
    ) -> Result<Option<Vec<KeySetInfo>>, Self::Err> {
        match self.mint_keysets.read().await.get(&mint_url) {
            Some(keyset_ids) => {
                let mut keysets = vec![];

                let db_keysets = self.keysets.read().await;

                for id in keyset_ids {
                    if let Some(keyset) = db_keysets.get(id) {
                        keysets.push(keyset.clone());
                    }
                }

                Ok(Some(keysets))
            }
            None => Ok(None),
        }
    }

    async fn get_keyset_by_id(&self, keyset_id: &Id) -> Result<Option<KeySetInfo>, Self::Err> {
        Ok(self.keysets.read().await.get(keyset_id).cloned())
    }

    async fn add_mint_quote(&self, quote: MintQuote) -> Result<(), Self::Err> {
        let event = mint_quote_to_event(&self.id, &quote, &self.keys).map_err(map_err)?;
        if let Some(db) = &self.db {
            db.save_event(&event).await.map_err(|e| map_err(e.into()))?;
        }
        self.client
            .send_event(event)
            .await
            .map_err(|e| map_err(e.into()))?;
        Ok(())
    }

    async fn get_mint_quote(&self, quote_id: &str) -> Result<Option<MintQuote>, Self::Err> {
        let quotes = self.get_mint_quotes().await?;
        Ok(quotes.into_iter().find(|quote| quote.id == quote_id))
    }

    async fn get_mint_quotes(&self) -> Result<Vec<MintQuote>, Self::Err> {
        let filters = vec![Filter {
            kinds: Some(vec![QUOTE_KIND].into_iter().collect()),
            ..Default::default()
        }];
        let events = unique_events(self.get_events(filters).await.map_err(map_err)?);
        let quotes = events
            .iter()
            .map(|event| mint_quote_from_event(event, &self.keys))
            .collect::<Result<Vec<MintQuote>, Error>>()
            .map_err(map_err)?;
        Ok(quotes)
    }

    async fn remove_mint_quote(&self, quote_id: &str) -> Result<(), Self::Err> {
        let filters = vec![Filter {
            kinds: Some(vec![QUOTE_KIND].into_iter().collect()),
            ..Default::default()
        }];
        let events = self.get_events(filters).await.map_err(map_err)?;
        for event in events {
            let quote = mint_quote_from_event(&event, &self.keys).map_err(map_err)?;
            if quote.id == quote_id {
                self.client
                    .delete_event(event.id())
                    .await
                    .map_err(|e| map_err(e.into()))?;
            }
        }
        Ok(())
    }

    async fn add_melt_quote(&self, quote: MeltQuote) -> Result<(), Self::Err> {
        let event = melt_quote_to_event(&self.id, &quote, &self.keys).map_err(map_err)?;
        if let Some(db) = &self.db {
            db.save_event(&event).await.map_err(|e| map_err(e.into()))?;
        }
        self.client
            .send_event(event)
            .await
            .map_err(|e| map_err(e.into()))?;
        Ok(())
    }

    async fn get_melt_quote(&self, quote_id: &str) -> Result<Option<MeltQuote>, Self::Err> {
        let filters = vec![Filter {
            kinds: Some(vec![QUOTE_KIND].into_iter().collect()),
            ..Default::default()
        }];
        let events = unique_events(self.get_events(filters).await.map_err(map_err)?);
        let quotes = events
            .iter()
            .map(|event| melt_quote_from_event(event, &self.keys))
            .collect::<Result<Vec<MeltQuote>, Error>>()
            .map_err(map_err)?;
        Ok(quotes.into_iter().find(|quote| quote.id == quote_id))
    }

    async fn remove_melt_quote(&self, quote_id: &str) -> Result<(), Self::Err> {
        let filters = vec![Filter {
            kinds: Some(vec![QUOTE_KIND].into_iter().collect()),
            ..Default::default()
        }];
        let events = self.get_events(filters).await.map_err(map_err)?;
        for event in events {
            let quote = melt_quote_from_event(&event, &self.keys).map_err(map_err)?;
            if quote.id == quote_id {
                self.client
                    .delete_event(event.id())
                    .await
                    .map_err(|e| map_err(e.into()))?;
            }
        }
        Ok(())
    }

    async fn add_keys(&self, keys: Keys) -> Result<(), Self::Err> {
        self.mint_keys.write().await.insert(Id::from(&keys), keys);
        Ok(())
    }

    async fn get_keys(&self, id: &Id) -> Result<Option<Keys>, Self::Err> {
        Ok(self.mint_keys.read().await.get(id).cloned())
    }

    async fn remove_keys(&self, id: &Id) -> Result<(), Self::Err> {
        self.mint_keys.write().await.remove(id);
        Ok(())
    }

    async fn get_proofs(
        &self,
        mint_url: Option<UncheckedUrl>,
        unit: Option<CurrencyUnit>,
        state: Option<Vec<State>>,
        spending_conditions: Option<Vec<SpendingConditions>>,
    ) -> Result<Vec<ProofInfo>, Self::Err> {
        let filters = vec![Filter {
            kinds: Some(vec![TX_KIND].into_iter().collect()),
            generic_tags: vec![(
                SingleLetterTag::from_char(ID_LINK_TAG)
                    .expect("ID_LINK_TAG is not a single letter tag"),
                vec![self.id.clone()].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        }];
        let events = unique_events(self.get_events(filters).await.map_err(map_err)?);
        let txs = events
            .iter()
            .map(|event| TxInfo::from_event(event, &self.keys))
            .collect::<Result<Vec<TxInfo>, Error>>()
            .map_err(map_err)?;
        let wallet_proofs = WalletProofs::from(txs);

        let mut proofs = Vec::new();
        for (wallet_mint_url, mint_proofs) in wallet_proofs.proofs {
            for proof in mint_proofs {
                let proof_state = self
                    .proof_states
                    .read()
                    .await
                    .get(&proof.y()?)
                    .cloned()
                    .unwrap_or(State::Unspent);
                if let Some(proof_unit) = self
                    .get_keyset_by_id(&proof.keyset_id)
                    .await?
                    .map(|ks| ks.unit)
                {
                    let info =
                        ProofInfo::new(proof, wallet_mint_url.clone(), proof_state, proof_unit)?;
                    if info.matches_conditions(&mint_url, &unit, &state, &spending_conditions) {
                        proofs.push(info);
                    }
                }
            }
        }
        Ok(proofs)
    }

    async fn update_proofs(
        &self,
        added: Vec<ProofInfo>,
        removed: Vec<ProofInfo>,
    ) -> Result<(), Self::Err> {
        let tx = TxInfo::new(added, removed);
        let event = tx.to_event(&self.id, &self.keys).map_err(map_err)?;
        if let Some(db) = &self.db {
            db.save_event(&event).await.map_err(|e| map_err(e.into()))?;
        }
        self.client
            .send_event(event)
            .await
            .map_err(|e| map_err(e.into()))?;
        Ok(())
    }

    async fn set_proof_state(&self, y: PublicKey, state: State) -> Result<(), Self::Err> {
        let mut proof_states = self.proof_states.write().await;
        proof_states.insert(y, state);
        Ok(())
    }

    async fn increment_keyset_counter(&self, keyset_id: &Id, count: u32) -> Result<(), Self::Err> {
        let mut info = self.get_wallet_info().await.map_err(map_err)?;
        let counter = info.counters.entry(keyset_id.clone()).or_insert(0);
        *counter += count;
        self.save_wallet_info(info).await.map_err(map_err)?;
        Ok(())
    }

    async fn get_keyset_counter(&self, id: &Id) -> Result<Option<u32>, Self::Err> {
        let info = self.get_wallet_info().await.map_err(map_err)?;
        Ok(info.counters.get(id).cloned())
    }

    async fn get_nostr_last_checked(
        &self,
        _verifying_key: &PublicKey,
    ) -> Result<Option<u32>, Self::Err> {
        let filters = vec![Filter {
            kinds: Some(vec![TX_KIND].into_iter().collect()),
            generic_tags: vec![(
                SingleLetterTag::from_char(ID_TAG).expect("ID_TAG is not a single letter tag"),
                vec![self.id.clone()].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        }];
        let events = self.get_events(filters).await.map_err(map_err)?;
        Ok(latest_event(events).map(|event| event.created_at.as_u64() as u32))
    }
    async fn add_nostr_last_checked(
        &self,
        _verifying_key: PublicKey,
        _last_checked: u32,
    ) -> Result<(), Self::Err> {
        Ok(())
    }
}

fn map_err(e: Error) -> super::Error {
    super::Error::Database(Box::new(e))
}

/// Wallet info
#[derive(Clone, Debug)]
pub struct WalletInfo {
    /// Wallet id
    pub id: String,
    /// Saved balance
    pub balance: Option<Amount>,
    /// List of mints
    pub mints: HashSet<UncheckedUrl>,
    /// Name
    pub name: Option<String>,
    /// Currency unit
    pub unit: Option<CurrencyUnit>,
    /// Description
    pub description: Option<String>,
    /// List of relays
    pub relays: HashSet<UncheckedUrl>,
    /// NIP-61 private key
    pub p2pk_priv_key: Option<SecretKey>,
    /// Key index counter
    pub counters: HashMap<Id, u32>,
}

impl WalletInfo {
    fn from_event(event: &Event, keys: &nostr_sdk::Keys) -> Result<Self, Error> {
        let id_tag = event.tags().iter().find(|tag| {
            tag.kind()
                == TagKind::SingleLetter(
                    SingleLetterTag::from_char(ID_TAG).expect("ID_TAG is not a single letter tag"),
                )
        });
        let id = id_tag
            .ok_or(Error::TagNotFound(ID_TAG.to_string()))?
            .content()
            .ok_or(Error::EmptyTag(ID_TAG.to_string()))?;
        let mut info = WalletInfo {
            id: id.to_string(),
            balance: None,
            mints: HashSet::new(),
            name: None,
            unit: None,
            description: None,
            relays: HashSet::new(),
            p2pk_priv_key: None,
            counters: HashMap::new(),
        };
        let content: Vec<Tag> = serde_json::from_str(&nip44::decrypt(
            keys.secret_key()?,
            &keys.public_key(),
            &event.content,
        )?)?;
        let mut tags = Vec::new();
        tags.extend(event.tags().to_vec());
        tags.extend(content);
        for tag in tags {
            match tag.kind().to_string().as_str() {
                MINT_TAG => {
                    info.mints.insert(UncheckedUrl::from_str(
                        tag.content().ok_or(Error::EmptyTag(MINT_TAG.to_string()))?,
                    )?);
                }
                RELAY_TAG => {
                    info.relays.insert(UncheckedUrl::from_str(
                        tag.content()
                            .ok_or(Error::EmptyTag(RELAY_TAG.to_string()))?,
                    )?);
                }
                UNIT_TAG => {
                    info.unit = Some(CurrencyUnit::from_str(
                        tag.content().ok_or(Error::EmptyTag(UNIT_TAG.to_string()))?,
                    )?);
                }
                NAME_TAG => {
                    info.name = Some(
                        tag.content()
                            .ok_or(Error::EmptyTag(NAME_TAG.to_string()))?
                            .to_string(),
                    );
                }
                DESCRIPTION_TAG => {
                    info.description = Some(
                        tag.content()
                            .ok_or(Error::EmptyTag(DESCRIPTION_TAG.to_string()))?
                            .to_string(),
                    );
                }
                BALANCE_TAG => {
                    let balance = tag
                        .content()
                        .ok_or(Error::EmptyTag(BALANCE_TAG.to_string()))?;
                    info.balance = Some(Amount::from(balance.parse::<u64>()?));
                }
                PRIVKEY_TAG => {
                    let priv_key = tag
                        .content()
                        .ok_or(Error::EmptyTag(PRIVKEY_TAG.to_string()))?;
                    info.p2pk_priv_key = Some(SecretKey::from_str(priv_key)?);
                }
                COUNTER_TAG => {
                    let id = Id::from_str(
                        tag.content()
                            .ok_or(Error::EmptyTag(COUNTER_TAG.to_string()))?,
                    )?;
                    let counter = tag
                        .as_vec()
                        .last()
                        .ok_or(Error::EmptyTag(COUNTER_TAG.to_string()))?
                        .parse::<u32>()?;
                    info.counters.insert(id, counter);
                }
                _ => {}
            }
        }
        Ok(info)
    }

    fn to_event(&self, keys: &nostr_sdk::Keys) -> Result<Event, Error> {
        let mut content = Vec::new();
        let tags = vec![Tag::parse(&[&ID_TAG.to_string(), &self.id])?];
        if let Some(balance) = &self.balance {
            if let Some(unit) = &self.unit {
                content.push(Tag::parse(&[
                    BALANCE_TAG,
                    &balance.to_string(),
                    &unit.to_string(),
                ])?);
                content.push(Tag::parse(&[UNIT_TAG, &unit.to_string()])?);
            } else {
                content.push(Tag::parse(&[BALANCE_TAG, &balance.to_string()])?);
            }
        }
        for mint in &self.mints {
            content.push(Tag::parse(&[MINT_TAG, &mint.to_string()])?);
        }
        if let Some(name) = &self.name {
            content.push(Tag::parse(&[NAME_TAG, name])?);
        }
        if let Some(description) = &self.description {
            content.push(Tag::parse(&[DESCRIPTION_TAG, description])?);
        }
        for relay in &self.relays {
            content.push(Tag::parse(&[RELAY_TAG, &relay.to_string()])?);
        }
        for (id, counter) in &self.counters {
            content.push(Tag::parse(&[
                COUNTER_TAG,
                &id.to_string(),
                &counter.to_string(),
            ])?);
        }
        let event = EventBuilder::new(
            WALLET_INFO_KIND,
            nip44::encrypt(
                keys.secret_key()?,
                &keys.public_key(),
                serde_json::to_string(&content)?,
                nip44::Version::V2,
            )?,
            tags,
        );
        Ok(event.to_event(keys)?)
    }
}

fn mint_quote_from_event(event: &Event, keys: &nostr_sdk::Keys) -> Result<MintQuote, Error> {
    let mut id: Option<String> = None;
    let mut mint_url: Option<UncheckedUrl> = None;
    let mut amount: Option<Amount> = None;
    let mut unit: Option<CurrencyUnit> = None;
    let mut request: Option<String> = None;
    let mut state: Option<MintQuoteState> = None;
    let mut expiry: Option<u64> = None;

    let mut tags = event.tags().to_vec();
    let content: Vec<Tag> = serde_json::from_str(&nip44::decrypt(
        keys.secret_key()?,
        &keys.public_key(),
        &event.content,
    )?)?;
    tags.extend(content);
    for tag in tags {
        match tag.kind().to_string().as_str() {
            QUOTE_ID_TAG => {
                id = Some(
                    tag.content()
                        .ok_or(Error::EmptyTag(QUOTE_ID_TAG.to_string()))?
                        .to_string(),
                );
            }
            MINT_TAG => {
                mint_url = Some(UncheckedUrl::from_str(
                    tag.content().ok_or(Error::EmptyTag(MINT_TAG.to_string()))?,
                )?);
            }
            AMOUNT_TAG => {
                amount = Some(Amount::from(
                    tag.content()
                        .ok_or(Error::EmptyTag(AMOUNT_TAG.to_string()))?
                        .parse::<u64>()?,
                ));
            }
            UNIT_TAG => {
                unit = Some(CurrencyUnit::from_str(
                    tag.content().ok_or(Error::EmptyTag(UNIT_TAG.to_string()))?,
                )?);
            }
            REQUEST_TAG => {
                request = Some(
                    tag.content()
                        .ok_or(Error::EmptyTag(REQUEST_TAG.to_string()))?
                        .to_string(),
                );
            }
            STATE_TAG => {
                state = Some(MintQuoteState::from_str(
                    tag.content()
                        .ok_or(Error::EmptyTag(STATE_TAG.to_string()))?,
                )?);
            }
            EXPIRATION_TAG => {
                expiry = Some(
                    tag.content()
                        .ok_or(Error::EmptyTag(EXPIRATION_TAG.to_string()))?
                        .parse::<u64>()?,
                );
            }
            _ => {}
        }
    }

    let quote = MintQuote {
        id: id.ok_or(Error::MissingTag(QUOTE_ID_TAG.to_string()))?,
        mint_url: mint_url.ok_or(Error::MissingTag(MINT_TAG.to_string()))?,
        amount: amount.ok_or(Error::MissingTag(AMOUNT_TAG.to_string()))?,
        unit: unit.ok_or(Error::MissingTag(UNIT_TAG.to_string()))?,
        request: request.ok_or(Error::MissingTag(REQUEST_TAG.to_string()))?,
        state: state.ok_or(Error::MissingTag(STATE_TAG.to_string()))?,
        expiry: expiry.ok_or(Error::MissingTag(EXPIRATION_TAG.to_string()))?,
    };
    Ok(quote)
}

fn mint_quote_to_event(
    wallet_id: &str,
    quote: &MintQuote,
    keys: &nostr_sdk::Keys,
) -> Result<Event, Error> {
    let mut content = Vec::new();
    let mut tags = Vec::new();
    content.push(Tag::parse(&[QUOTE_ID_TAG, &quote.id])?);
    content.push(Tag::parse(&[MINT_TAG, &quote.mint_url.to_string()])?);
    content.push(Tag::parse(&[AMOUNT_TAG, &quote.amount.to_string()])?);
    content.push(Tag::parse(&[UNIT_TAG, &quote.unit.to_string()])?);
    content.push(Tag::parse(&[REQUEST_TAG, &quote.request])?);
    tags.push(wallet_link_tag(wallet_id, keys)?);
    tags.push(Tag::parse(&[
        &EXPIRATION_TAG.to_string(),
        &quote.expiry.to_string(),
    ])?);
    let event = EventBuilder::new(
        QUOTE_KIND,
        nip44::encrypt(
            keys.secret_key()?,
            &keys.public_key(),
            serde_json::to_string(&content)?,
            nip44::Version::V2,
        )?,
        tags,
    );
    Ok(event.to_event(keys)?)
}

fn melt_quote_from_event(event: &Event, keys: &nostr_sdk::Keys) -> Result<MeltQuote, Error> {
    let mut id: Option<String> = None;
    let mut amount: Option<Amount> = None;
    let mut unit: Option<CurrencyUnit> = None;
    let mut request: Option<String> = None;
    let mut fee_reserve: Option<Amount> = None;
    let mut preimage: Option<String> = None;
    let mut expiry: Option<u64> = None;

    let mut tags = event.tags().to_vec();
    let content: Vec<Tag> = serde_json::from_str(&nip44::decrypt(
        keys.secret_key()?,
        &keys.public_key(),
        &event.content,
    )?)?;
    tags.extend(content);
    for tag in tags {
        match tag.kind().to_string().as_str() {
            QUOTE_ID_TAG => {
                id = Some(
                    tag.content()
                        .ok_or(Error::EmptyTag(QUOTE_ID_TAG.to_string()))?
                        .to_string(),
                );
            }
            AMOUNT_TAG => {
                amount = Some(Amount::from(
                    tag.content()
                        .ok_or(Error::EmptyTag(AMOUNT_TAG.to_string()))?
                        .parse::<u64>()?,
                ));
            }
            UNIT_TAG => {
                unit = Some(CurrencyUnit::from_str(
                    tag.content().ok_or(Error::EmptyTag(UNIT_TAG.to_string()))?,
                )?);
            }
            REQUEST_TAG => {
                request = Some(
                    tag.content()
                        .ok_or(Error::EmptyTag(REQUEST_TAG.to_string()))?
                        .to_string(),
                );
            }
            FEE_RESERVE => {
                fee_reserve = Some(Amount::from(
                    tag.content()
                        .ok_or(Error::EmptyTag(FEE_RESERVE.to_string()))?
                        .parse::<u64>()?,
                ));
            }
            PREIMAGE_TAG => {
                preimage = Some(
                    tag.content()
                        .ok_or(Error::EmptyTag(PREIMAGE_TAG.to_string()))?
                        .to_string(),
                );
            }
            EXPIRATION_TAG => {
                expiry = Some(
                    tag.content()
                        .ok_or(Error::EmptyTag(EXPIRATION_TAG.to_string()))?
                        .parse::<u64>()?,
                );
            }
            _ => {}
        }
    }

    let quote = MeltQuote {
        id: id.ok_or(Error::MissingTag(QUOTE_ID_TAG.to_string()))?,
        amount: amount.ok_or(Error::MissingTag(AMOUNT_TAG.to_string()))?,
        unit: unit.ok_or(Error::MissingTag(UNIT_TAG.to_string()))?,
        request: request.ok_or(Error::MissingTag(REQUEST_TAG.to_string()))?,
        fee_reserve: fee_reserve.ok_or(Error::MissingTag(FEE_RESERVE.to_string()))?,
        state: MeltQuoteState::Pending,
        expiry: expiry.ok_or(Error::MissingTag(EXPIRATION_TAG.to_string()))?,
        payment_preimage: preimage,
    };
    Ok(quote)
}

fn melt_quote_to_event(
    wallet_id: &str,
    quote: &MeltQuote,
    keys: &nostr_sdk::Keys,
) -> Result<Event, Error> {
    let mut content = Vec::new();
    let mut tags = Vec::new();
    content.push(Tag::parse(&[QUOTE_ID_TAG, &quote.id])?);
    content.push(Tag::parse(&[AMOUNT_TAG, &quote.amount.to_string()])?);
    content.push(Tag::parse(&[UNIT_TAG, &quote.unit.to_string()])?);
    content.push(Tag::parse(&[REQUEST_TAG, &quote.request])?);
    tags.push(wallet_link_tag(wallet_id, keys)?);
    tags.push(Tag::parse(&[
        &EXPIRATION_TAG.to_string(),
        &quote.expiry.to_string(),
    ])?);
    let event = EventBuilder::new(
        QUOTE_KIND,
        nip44::encrypt(
            keys.secret_key()?,
            &keys.public_key(),
            serde_json::to_string(&content)?,
            nip44::Version::V2,
        )?,
        tags,
    );
    Ok(event.to_event(keys)?)
}

#[derive(Debug, Serialize, Deserialize)]
struct TxInfo {
    inputs: Vec<EventProofs>,
    outputs: Vec<EventProofs>,
}

impl TxInfo {
    fn new(inputs: Vec<ProofInfo>, outputs: Vec<ProofInfo>) -> Self {
        let inputs = inputs
            .into_iter()
            .chunk_by(|proof| proof.mint_url.clone())
            .into_iter()
            .map(|(mint_url, chunk)| EventProofs {
                mint_url,
                proofs: chunk.into_iter().map(|proof| proof.proof).collect(),
            })
            .collect();
        let outputs = outputs
            .into_iter()
            .chunk_by(|proof| proof.mint_url.clone())
            .into_iter()
            .map(|(mint_url, chunk)| EventProofs {
                mint_url,
                proofs: chunk.into_iter().map(|proof| proof.proof).collect(),
            })
            .collect();
        Self { inputs, outputs }
    }

    fn from_event(event: &Event, keys: &nostr_sdk::Keys) -> Result<Self, Error> {
        Ok(serde_json::from_str(&nip44::decrypt(
            keys.secret_key()?,
            &keys.public_key(),
            &event.content,
        )?)?)
    }

    fn to_event(&self, wallet_id: &str, keys: &nostr_sdk::Keys) -> Result<Event, Error> {
        let mut tags = Vec::new();
        tags.push(wallet_link_tag(wallet_id, keys)?);

        let event = EventBuilder::new(
            TX_KIND,
            nip44::encrypt(
                keys.secret_key()?,
                &keys.public_key(),
                serde_json::to_string(&self)?,
                nip44::Version::V2,
            )?,
            tags,
        );
        Ok(event.to_event(keys)?)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct EventProofs {
    mint_url: UncheckedUrl,
    proofs: Vec<Proof>,
}

struct WalletProofs {
    proofs: HashMap<UncheckedUrl, Vec<Proof>>,
}

impl WalletProofs {
    fn update(&mut self, inputs: Vec<EventProofs>, outputs: Vec<EventProofs>) {
        for input in inputs {
            self.proofs
                .entry(input.mint_url)
                .and_modify(|proofs| {
                    for proof in &input.proofs {
                        proofs.push(proof.clone());
                    }
                })
                .or_insert(input.proofs);
        }
        for output in outputs {
            self.proofs
                .entry(output.mint_url)
                .and_modify(|proofs| {
                    for proof in &output.proofs {
                        proofs.push(proof.clone());
                    }
                })
                .or_insert(output.proofs);
        }
    }
}

impl From<Vec<TxInfo>> for WalletProofs {
    fn from(txs: Vec<TxInfo>) -> Self {
        let mut self_ = Self {
            proofs: HashMap::new(),
        };
        for tx in txs {
            self_.update(tx.outputs, tx.inputs);
        }
        self_
    }
}

fn wallet_link_tag(wallet_id: &str, keys: &nostr_sdk::Keys) -> Result<Tag, Error> {
    Ok(Tag::parse(&[
        &ID_LINK_TAG.to_string(),
        &format!("{}:{}:{}", WALLET_INFO_KIND, keys.public_key(), wallet_id),
    ])?)
}

/// WalletNostrDatabase error
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Client error
    #[error(transparent)]
    Client(#[from] client::Error),
    /// Database error
    #[error(transparent)]
    Database(#[from] DatabaseError),
    /// Empty tag error
    #[error("Empty tag: {0}")]
    EmptyTag(String),
    /// Encrypt error
    #[error(transparent)]
    Encrypt(#[from] nip44::Error),
    /// Event error
    #[error(transparent)]
    Event(#[from] nostr_sdk::event::Error),
    /// Event builder error
    #[error(transparent)]
    EventBuilder(#[from] nostr_sdk::event::builder::Error),
    /// Json error
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Key error
    #[error(transparent)]
    Key(#[from] nostr_sdk::key::Error),
    /// Missing tag error
    #[error("Missing tag: {0}")]
    MissingTag(String),
    /// Event not found
    #[error("Event not found")]
    NotFound,
    /// NUT-00 error
    #[error(transparent)]
    Nut00(#[from] crate::nuts::nut00::Error),
    /// NUT-02 error
    #[error(transparent)]
    Nut02(#[from] crate::nuts::nut02::Error),
    /// NUT-04 error
    #[error(transparent)]
    Nut04(#[from] crate::nuts::nut04::Error),
    /// Parse int error
    #[error(transparent)]
    ParseInt(#[from] std::num::ParseIntError),
    /// Subscribe error
    #[error(transparent)]
    Subscribe(#[from] tokio::sync::broadcast::error::RecvError),
    /// Tag error
    #[error(transparent)]
    Tag(#[from] nostr_sdk::event::tag::Error),
    /// Tag not found
    #[error("Tag not found: {0}")]
    TagNotFound(String),
    /// Url parse error
    #[error(transparent)]
    UrlParse(#[from] crate::url::Error),
}
