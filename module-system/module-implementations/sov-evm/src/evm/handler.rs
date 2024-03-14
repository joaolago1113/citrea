use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::sync::Arc;

use revm::handler::register::EvmHandler;
use revm::interpreter::InstructionResult;
use revm::primitives::{Address, EVMError, ResultAndState, B256, U256};
use revm::{Context, Database, FrameResult, JournalEntry};

pub(crate) trait CitreaExternal {
    fn l1_fee_rate(&self) -> u64;
}
pub(crate) struct CitreaExternalContext {
    l1_fee_rate: u64,
}

impl CitreaExternalContext {
    pub(crate) fn new(l1_fee_rate: u64) -> Self {
        Self { l1_fee_rate }
    }
}

impl CitreaExternal for CitreaExternalContext {
    fn l1_fee_rate(&self) -> u64 {
        self.l1_fee_rate
    }
}

pub(crate) fn citrea_handle_register<DB, EXT>(handler: &mut EvmHandler<'_, EXT, DB>)
where
    DB: Database,
    EXT: CitreaExternal,
{
    let post_execution = &mut handler.post_execution;
    post_execution.output = Arc::new(CitreaHandler::<EXT, DB>::post_execution_output);
}

struct CitreaHandler<EXT, DB> {
    _phantom: std::marker::PhantomData<(EXT, DB)>,
}

impl<EXT: CitreaExternal, DB: Database> CitreaHandler<EXT, DB> {
    fn post_execution_output(
        context: &mut Context<EXT, DB>,
        result: FrameResult,
    ) -> Result<ResultAndState, EVMError<<DB as Database>::Error>> {
        if !result.interpreter_result().is_error() {
            let diff_size = U256::from(calc_diff_size(context).map_err(EVMError::Database)?);
            let l1_fee_rate = U256::from(context.external.l1_fee_rate());
            let l1_fee = diff_size * l1_fee_rate;
            if let Some(_out_of_funds) = decrease_caller_balance(context, l1_fee)? {
                return Err(EVMError::Custom(format!(
                    "Not enought funds for L1 fee: {}",
                    l1_fee
                )));
            }
        }

        revm::handler::mainnet::output(context, result)
    }
}

/// Calculates the diff of the modified state.
fn calc_diff_size<EXT, DB: Database>(
    context: &mut Context<EXT, DB>,
) -> Result<usize, <DB as Database>::Error> {
    // Get the last journal entry to calculate diff.
    let journal = context
        .evm
        .journaled_state
        .journal
        .last()
        .cloned()
        .unwrap_or(vec![]);
    let state = &context.evm.journaled_state.state;

    #[derive(Default)]
    struct AccountChange<'a> {
        created: bool,
        destroyed: bool,
        nonce_changed: bool,
        code_changed: bool,
        balance_changed: bool,
        storage_changes: HashSet<&'a U256>,
    }

    let mut account_changes: HashMap<&Address, AccountChange<'_>> = HashMap::new();

    for entry in &journal {
        match entry {
            JournalEntry::NonceChange { address } => {
                let account = account_changes.entry(address).or_default();
                account.nonce_changed = true;
            }
            JournalEntry::BalanceTransfer { from, to, .. } => {
                let from = account_changes.entry(from).or_default();
                from.balance_changed = true;
                let to = account_changes.entry(to).or_default();
                to.balance_changed = true;
            }
            JournalEntry::StorageChange { address, key, .. } => {
                let account = account_changes.entry(address).or_default();
                account.storage_changes.insert(key);
            }
            JournalEntry::CodeChange { address } => {
                let account = account_changes.entry(address).or_default();
                account.code_changed = true;
            }
            JournalEntry::AccountCreated { address } => {
                let account = account_changes.entry(address).or_default();
                account.created = true;
                // When account is created, there is a transfer to init its balance.
                // So we need to only force the nonce change.
                account.nonce_changed = true;
            }
            JournalEntry::AccountDestroyed { address, .. } => {
                let account = account_changes.entry(address).or_default();
                if account.created {
                    // That's a temporary account.
                    // Delete it from the account changes to enable cancun support.
                    // Acc with the same address can be created again in the same tx.
                    account_changes.remove(address);
                } else {
                    account.destroyed = true;
                }
            }
            _ => {}
        }
    }

    let slot_size = 2 * size_of::<U256>(); // key + value;
    let mut diff_size = 0usize;

    for (addr, account) in account_changes {
        // Apply size of address of changed account
        diff_size += size_of::<Address>();

        if account.destroyed {
            let account = &state[addr];
            diff_size += slot_size * account.storage.len(); // Storage size
            diff_size += size_of::<u64>(); // Nonces are u64
            diff_size += size_of::<U256>(); // Balances are U256
            diff_size += size_of::<B256>(); // Code hashes are B256

            // Retrieve code from DB and apply its size
            if let Some(info) = context.evm.db.basic(*addr)? {
                if let Some(code) = info.code {
                    diff_size += code.len();
                } else {
                    let code = context.evm.db.code_by_hash(info.code_hash)?;
                    diff_size += code.len();
                }
            }
            continue;
        }

        // Apply size of changed nonce
        if account.nonce_changed {
            diff_size += size_of::<u64>(); // Nonces are u64
        }

        // Apply size of changed balances
        if account.balance_changed {
            diff_size += size_of::<U256>(); // Balances are U256
        }

        // Apply size of changed slots
        diff_size += slot_size * account.storage_changes.len();

        // Apply size of changed codes
        if account.code_changed {
            let account = &state[addr];
            diff_size += size_of::<B256>(); // Code hashes are B256
            if let Some(code) = account.info.code.as_ref() {
                diff_size += code.len()
            } else {
                tracing::warn!(
                    "Code must exist for account when calculating diff: {}",
                    addr,
                );
            }
        }
    }

    Ok(diff_size)
}

/// Decreases the balance of the caller by the given amount.
/// Returns Ok(Some) if the caller's balance is not enough.
fn decrease_caller_balance<EXT, DB: Database>(
    context: &mut Context<EXT, DB>,
    amount: U256,
) -> Result<Option<InstructionResult>, EVMError<DB::Error>> {
    let caller = context.evm.env.tx.caller;

    let (caller_account, _) = context
        .evm
        .journaled_state
        .load_account(caller, &mut context.evm.db)?;

    let balance = &mut caller_account.info.balance;

    let Some(new_balance) = balance.checked_sub(amount) else {
        return Ok(Some(InstructionResult::OutOfFunds));
    };

    *balance = new_balance;

    Ok(None)
}