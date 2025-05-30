use async_graphql::{dataloader::*, types::connection::*, *};

use cala_ledger::{
    account_set::AccountSetMemberId,
    balance::*,
    entry::EntriesByCreatedAtCursor,
    primitives::{AccountId, AccountSetId, Currency, JournalId},
};

pub use cala_ledger::account_set::{AccountSetMembersByCreatedAtCursor, AccountSetsByNameCursor};

use super::{
    balance::*, convert::ToGlobalId, loader::LedgerDataLoader, primitives::*, schema::DbOp,
};
use crate::app::CalaApp;

#[derive(Union)]
enum AccountSetMember {
    Account(super::account::Account),
    AccountSet(AccountSet),
}

#[derive(Clone, SimpleObject)]
#[graphql(complex)]
pub struct AccountSet {
    id: ID,
    account_set_id: UUID,
    version: u32,
    journal_id: UUID,
    name: String,
    normal_balance_type: DebitOrCredit,
    description: Option<String>,
    metadata: Option<JSON>,
    created_at: Timestamp,
    modified_at: Timestamp,
}

#[derive(SimpleObject, Clone)]
pub struct AccountSetTransactionEvent {
        /// Event UUID
    pub id: UUID,
    /// Event type (e.g., "initialized", "transfer", etc.)
    pub event_type: String,
    /// Amount (as string for precision)
    pub units: String,
    /// Currency code (e.g., "NGN", "BTC")
    pub currency: String,
    /// Direction (e.g., "debit", "credit")
    pub direction: String,
    /// Transaction UUID
    pub transaction_id: UUID,
    /// Event creation time
    pub created_at: Timestamp,
}


#[ComplexObject]
impl AccountSet {
    async fn balance(
        &self,
        ctx: &Context<'_>,
        currency: CurrencyCode,
    ) -> async_graphql::Result<Option<Balance>> {
        let journal_id = JournalId::from(self.journal_id);
        let account_id = AccountId::from(self.account_set_id);
        let currency = Currency::from(currency);

        let balance: Option<AccountBalance> = match ctx.data_opt::<DbOp>() {
            Some(op) => {
                let app = ctx.data_unchecked::<CalaApp>();
                let mut op = op.try_lock().expect("Lock held concurrently");
                Some(
                    app.ledger()
                        .balances()
                        .find_in_op(&mut op, journal_id, account_id, currency)
                        .await?,
                )
            }
            None => {
                let loader = ctx.data_unchecked::<DataLoader<LedgerDataLoader>>();
                loader.load_one((journal_id, account_id, currency)).await?
            }
        };
        Ok(balance.map(Balance::from))
    }

        /// Paginated transaction events for this account (deposits, withdrawals, trades, etc.)
    async fn transactions(
        &self,
        ctx: &Context<'_>,
        after: Option<String>,
        first: Option<i32>,
    ) -> async_graphql::Result<Connection<String, AccountSetTransactionEvent>> {
        let account_set_id = AccountSetId::from(self.account_set_id);
        let limit = first.unwrap_or(20).min(100);
        
        // Parse the cursor from the string if provided
        let cursor = after.map(|s| serde_json::from_str::<EntriesByCreatedAtCursor>(&s))
                        .transpose()?;
        
        // Create pagination query args
        let query_args = es_entity::PaginatedQueryArgs {
            first: limit as usize,
            after: cursor,
        };
        
        // Use descending order to get newest transactions first
        let direction = es_entity::ListDirection::Descending;
        
        // Get entries using the existing pattern from balance()
        let entries_result = match ctx.data_opt::<DbOp>() {
            Some(op) => {
                let app = ctx.data_unchecked::<CalaApp>();
                let _op = op.try_lock().expect("Lock held concurrently");
                // We need to implement this method or use an equivalent
                app.ledger()
                    .entries()
                    .list_for_account_set_id(account_set_id, query_args, direction)
                    .await?
            }
            None => {
                // If we're outside a transaction, use the app directly
                let app = ctx.data_unchecked::<CalaApp>();
                app.ledger()
                    .entries()
                    .list_for_account_set_id(account_set_id, query_args, direction)
                    .await?
            }
        };
        
        // Convert entries to GraphQL connection
        let mut connection = Connection::new(false, entries_result.has_next_page);
        
        // Transform entries into AccountSetTransactionEvent objects
        for entry in entries_result.entities {
            let entry_values = entry.values();
            
            // Create a transaction event from the entry using actual values from the entry
            let event = AccountSetTransactionEvent {
                id: UUID::from(entry.id()),
                event_type: format!("entry:{}", entry_values.id),
                units: entry_values.units.to_string(), // Use actual units
                currency: entry_values.currency.to_string(), // Use actual currency
                direction: entry_values.direction.to_string().to_lowercase(), // Use actual direction
                transaction_id: UUID::from(entry_values.transaction_id),
                created_at: entry.created_at().into(),
            };
            
            // Use a simple numeric string as the cursor
            let cursor = entry_values.id.to_string(); 
            connection.edges.push(Edge::new(cursor, event));
        }
        
        Ok(connection)
    }

    async fn members(
        &self,
        ctx: &Context<'_>,
        first: i32,
        after: Option<String>,
    ) -> Result<
        Connection<AccountSetMembersByCreatedAtCursor, AccountSetMember, EmptyFields, EmptyFields>,
    > {
        let app = ctx.data_unchecked::<CalaApp>();
        let account_set_id = AccountSetId::from(self.account_set_id);

        query(
            after.clone(),
            None,
            Some(first),
            None,
            |after, _, first, _| async move {
                let first = first.expect("First always exists");
                let query_args = cala_ledger::es_entity::PaginatedQueryArgs { first, after };

                let (members, mut accounts, mut sets) = match ctx.data_opt::<DbOp>() {
                    Some(op) => {
                        let mut op = op.try_lock().expect("Lock held concurrently");
                        let account_sets = app.ledger().account_sets();
                        let accounts = app.ledger().accounts();
                        let members = account_sets
                            .list_members_by_created_at_in_op(&mut op, account_set_id, query_args)
                            .await?;
                        let mut account_ids = Vec::new();
                        let mut set_ids = Vec::new();
                        for member in members.entities.iter() {
                            match member.id {
                                AccountSetMemberId::Account(id) => account_ids.push(id),
                                AccountSetMemberId::AccountSet(id) => set_ids.push(id),
                            }
                        }
                        (
                            members,
                            accounts.find_all_in_op(&mut op, &account_ids).await?,
                            account_sets.find_all_in_op(&mut op, &set_ids).await?,
                        )
                    }
                    None => {
                        let members = app
                            .ledger()
                            .account_sets()
                            .list_members_by_created_at(account_set_id, query_args)
                            .await?;
                        let mut account_ids = Vec::new();
                        let mut set_ids = Vec::new();
                        for member in members.entities.iter() {
                            match member.id {
                                AccountSetMemberId::Account(id) => account_ids.push(id),
                                AccountSetMemberId::AccountSet(id) => set_ids.push(id),
                            }
                        }
                        let loader = ctx.data_unchecked::<DataLoader<LedgerDataLoader>>();
                        (
                            members,
                            loader.load_many(account_ids).await?,
                            loader.load_many(set_ids).await?,
                        )
                    }
                };
                let mut connection = Connection::new(false, members.has_next_page);
                connection.edges.extend(members.entities.into_iter().map(
                    |member| match member.id {
                        AccountSetMemberId::Account(id) => {
                            let entity = accounts.remove(&id).expect("Account exists");
                            let cursor = AccountSetMembersByCreatedAtCursor::from(&member);
                            Edge::new(cursor, AccountSetMember::Account(entity))
                        }
                        AccountSetMemberId::AccountSet(id) => {
                            let entity = sets.remove(&id).expect("Account exists");
                            let cursor = AccountSetMembersByCreatedAtCursor::from(&member);
                            Edge::new(cursor, AccountSetMember::AccountSet(entity))
                        }
                    },
                ));
                Ok::<_, async_graphql::Error>(connection)
            },
        )
        .await
    }

    async fn sets(
        &self,
        ctx: &Context<'_>,
        first: i32,
        after: Option<String>,
    ) -> Result<Connection<AccountSetsByNameCursor, AccountSet, EmptyFields, EmptyFields>> {
        let app = ctx.data_unchecked::<CalaApp>();
        let account_set_id = AccountSetId::from(self.account_set_id);

        query(
            after.clone(),
            None,
            Some(first),
            None,
            |after, _, first, _| async move {
                let first = first.expect("First always exists");
                let query_args = cala_ledger::es_entity::PaginatedQueryArgs { first, after };

                let result = match ctx.data_opt::<DbOp>() {
                    Some(op) => {
                        let mut op = op.try_lock().expect("Lock held concurrently");
                        app.ledger()
                            .account_sets()
                            .find_where_member_in_op(&mut op, account_set_id, query_args)
                            .await?
                    }
                    None => {
                        app.ledger()
                            .account_sets()
                            .find_where_member(account_set_id, query_args)
                            .await?
                    }
                };

                let mut connection = Connection::new(false, result.has_next_page);
                connection
                    .edges
                    .extend(result.entities.into_iter().map(|entity| {
                        let cursor = AccountSetsByNameCursor::from(&entity);
                        Edge::new(cursor, AccountSet::from(entity))
                    }));

                Ok::<_, async_graphql::Error>(connection)
            },
        )
        .await
    }
}

#[derive(InputObject)]
pub(super) struct AccountSetCreateInput {
    pub account_set_id: UUID,
    pub journal_id: UUID,
    pub name: String,
    #[graphql(default)]
    pub normal_balance_type: DebitOrCredit,
    pub description: Option<String>,
    pub metadata: Option<JSON>,
}

#[derive(SimpleObject)]
pub(super) struct AccountSetCreatePayload {
    pub account_set: AccountSet,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum AccountSetMemberType {
    Account,
    AccountSet,
}

#[derive(InputObject)]
pub(super) struct AddToAccountSetInput {
    pub account_set_id: UUID,
    pub member_id: UUID,
    pub member_type: AccountSetMemberType,
}

impl From<AddToAccountSetInput> for AccountSetMemberId {
    fn from(input: AddToAccountSetInput) -> Self {
        match input.member_type {
            AccountSetMemberType::Account => {
                AccountSetMemberId::Account(AccountId::from(input.member_id))
            }
            AccountSetMemberType::AccountSet => {
                AccountSetMemberId::AccountSet(AccountSetId::from(input.member_id))
            }
        }
    }
}

#[derive(SimpleObject)]
pub(super) struct AddToAccountSetPayload {
    pub account_set: AccountSet,
}

#[derive(InputObject)]
pub(super) struct RemoveFromAccountSetInput {
    pub account_set_id: UUID,
    pub member_id: UUID,
    pub member_type: AccountSetMemberType,
}

impl From<RemoveFromAccountSetInput> for AccountSetMemberId {
    fn from(input: RemoveFromAccountSetInput) -> Self {
        match input.member_type {
            AccountSetMemberType::Account => {
                AccountSetMemberId::Account(AccountId::from(input.member_id))
            }
            AccountSetMemberType::AccountSet => {
                AccountSetMemberId::AccountSet(AccountSetId::from(input.member_id))
            }
        }
    }
}

#[derive(SimpleObject)]
pub(super) struct RemoveFromAccountSetPayload {
    pub account_set: AccountSet,
}

impl ToGlobalId for cala_ledger::AccountSetId {
    fn to_global_id(&self) -> async_graphql::types::ID {
        async_graphql::types::ID::from(format!("account_set:{}", self))
    }
}

impl From<cala_ledger::account_set::AccountSet> for AccountSet {
    fn from(account_set: cala_ledger::account_set::AccountSet) -> Self {
        let created_at = account_set.created_at();
        let modified_at = account_set.modified_at();
        let values = account_set.into_values();
        Self {
            id: values.id.to_global_id(),
            account_set_id: UUID::from(values.id),
            version: values.version,
            journal_id: UUID::from(values.journal_id),
            name: values.name,
            normal_balance_type: values.normal_balance_type,
            description: values.description,
            metadata: values.metadata.map(JSON::from),
            created_at: created_at.into(),
            modified_at: modified_at.into(),
        }
    }
}

impl From<cala_ledger::account_set::AccountSet> for AccountSetCreatePayload {
    fn from(value: cala_ledger::account_set::AccountSet) -> Self {
        Self {
            account_set: AccountSet::from(value),
        }
    }
}

impl From<cala_ledger::account_set::AccountSet> for AddToAccountSetPayload {
    fn from(value: cala_ledger::account_set::AccountSet) -> Self {
        Self {
            account_set: AccountSet::from(value),
        }
    }
}

impl From<cala_ledger::account_set::AccountSet> for RemoveFromAccountSetPayload {
    fn from(value: cala_ledger::account_set::AccountSet) -> Self {
        Self {
            account_set: AccountSet::from(value),
        }
    }
}

#[derive(InputObject)]
pub(super) struct AccountSetUpdateInput {
    pub name: Option<String>,
    pub normal_balance_type: Option<DebitOrCredit>,
    pub description: Option<String>,
    pub metadata: Option<JSON>,
}

#[derive(SimpleObject)]
pub(super) struct AccountSetUpdatePayload {
    pub account_set: AccountSet,
}

impl From<cala_ledger::account_set::AccountSet> for AccountSetUpdatePayload {
    fn from(value: cala_ledger::account_set::AccountSet) -> Self {
        Self {
            account_set: AccountSet::from(value),
        }
    }
}
