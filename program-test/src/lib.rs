//! The solana-program-test provides a BanksClient-based test framework BPF programs
#![allow(clippy::integer_arithmetic)]

#[allow(deprecated)]
use solana_sdk::sysvar::fees::Fees;
use {
    async_trait::async_trait,
    chrono_humanize::{Accuracy, HumanTime, Tense},
    log::*,
    solana_banks_client::start_client,
    solana_banks_server::banks_server::start_local_server,
    solana_program_runtime::instruction_processor::InstructionProcessor,
    solana_runtime::{
        bank::{Bank, ExecuteTimings},
        bank_forks::BankForks,
        builtins::Builtin,
        commitment::BlockCommitmentCache,
        genesis_utils::{create_genesis_config_with_leader_ex, GenesisConfigInfo},
    },
    solana_sdk::{
        account::{Account, AccountSharedData, ReadableAccount, WritableAccount},
        account_info::AccountInfo,
        clock::{Clock, Slot},
        compute_budget::ComputeBudget,
        entrypoint::{ProgramResult, SUCCESS},
        epoch_schedule::EpochSchedule,
        feature_set::demote_program_write_locks,
        fee_calculator::{FeeCalculator, FeeRateGovernor},
        genesis_config::{ClusterType, GenesisConfig},
        hash::Hash,
        instruction::Instruction,
        instruction::InstructionError,
        message::Message,
        native_token::sol_to_lamports,
        poh_config::PohConfig,
        process_instruction::{stable_log, InvokeContext, ProcessInstructionWithContext},
        program_error::{ProgramError, ACCOUNT_BORROW_FAILED, UNSUPPORTED_SYSVAR},
        pubkey::Pubkey,
        rent::Rent,
        signature::{Keypair, Signer},
        sysvar::{
            clock, epoch_schedule,
            fees::{self},
            rent, Sysvar,
        },
    },
    solana_vote_program::vote_state::{VoteState, VoteStateVersions},
    std::{
        cell::RefCell,
        collections::HashMap,
        convert::TryFrom,
        fs::File,
        io::{self, Read},
        mem::transmute,
        path::{Path, PathBuf},
        rc::Rc,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        time::{Duration, Instant},
    },
    thiserror::Error,
    tokio::task::JoinHandle,
};

// Export types so test clients can limit their solana crate dependencies
pub use solana_banks_client::BanksClient;

// Export tokio for test clients
pub use tokio;

pub mod programs;

#[macro_use]
extern crate solana_bpf_loader_program;

/// Errors from the program test environment
#[derive(Error, Debug, PartialEq)]
pub enum ProgramTestError {
    /// The chosen warp slot is not in the future, so warp is not performed
    #[error("Warp slot not in the future")]
    InvalidWarpSlot,
}

thread_local! {
    static INVOKE_CONTEXT: RefCell<Option<(usize, usize)>> = RefCell::new(None);
}
fn set_invoke_context(new: &mut dyn InvokeContext) {
    INVOKE_CONTEXT.with(|invoke_context| unsafe {
        invoke_context.replace(Some(transmute::<_, (usize, usize)>(new)))
    });
}
fn get_invoke_context<'a>() -> &'a mut dyn InvokeContext {
    let fat = INVOKE_CONTEXT.with(|invoke_context| match *invoke_context.borrow() {
        Some(val) => val,
        None => panic!("Invoke context not set!"),
    });
    unsafe { transmute::<(usize, usize), &mut dyn InvokeContext>(fat) }
}

pub fn builtin_process_instruction(
    process_instruction: solana_sdk::entrypoint::ProcessInstruction,
    _first_instruction_account: usize,
    input: &[u8],
    invoke_context: &mut dyn InvokeContext,
) -> Result<(), InstructionError> {
    set_invoke_context(invoke_context);

    let logger = invoke_context.get_logger();
    let program_id = invoke_context.get_caller()?;
    stable_log::program_invoke(&logger, program_id, invoke_context.invoke_depth());

    // Skip the processor account
    let keyed_accounts = &invoke_context.get_keyed_accounts()?[1..];

    // Copy all the accounts into a HashMap to ensure there are no duplicates
    let mut accounts: HashMap<Pubkey, Account> = keyed_accounts
        .iter()
        .map(|ka| {
            (
                *ka.unsigned_key(),
                Account::from(ka.account.borrow().clone()),
            )
        })
        .collect();

    // Create shared references to each account's lamports/data/owner
    let account_refs: HashMap<_, _> = accounts
        .iter_mut()
        .map(|(key, account)| {
            (
                *key,
                (
                    Rc::new(RefCell::new(&mut account.lamports)),
                    Rc::new(RefCell::new(&mut account.data[..])),
                    &account.owner,
                ),
            )
        })
        .collect();

    // Create AccountInfos
    let account_infos: Vec<AccountInfo> = keyed_accounts
        .iter()
        .map(|keyed_account| {
            let key = keyed_account.unsigned_key();
            let (lamports, data, owner) = &account_refs[key];
            AccountInfo {
                key,
                is_signer: keyed_account.signer_key().is_some(),
                is_writable: keyed_account.is_writable(),
                lamports: lamports.clone(),
                data: data.clone(),
                owner,
                executable: keyed_account.executable().unwrap(),
                rent_epoch: keyed_account.rent_epoch().unwrap(),
            }
        })
        .collect();

    // Execute the program
    process_instruction(program_id, &account_infos, input).map_err(|err| {
        let err = u64::from(err);
        stable_log::program_failure(&logger, program_id, &err.into());
        err
    })?;
    stable_log::program_success(&logger, program_id);

    // Commit AccountInfo changes back into KeyedAccounts
    for keyed_account in keyed_accounts {
        let mut account = keyed_account.account.borrow_mut();
        let key = keyed_account.unsigned_key();
        let (lamports, data, _owner) = &account_refs[key];
        account.set_lamports(**lamports.borrow());
        account.set_data(data.borrow().to_vec());
    }

    Ok(())
}

/// Converts a `solana-program`-style entrypoint into the runtime's entrypoint style, for
/// use with `ProgramTest::add_program`
#[macro_export]
macro_rules! processor {
    ($process_instruction:expr) => {
        Some(
            |first_instruction_account: usize,
             input: &[u8],
             invoke_context: &mut dyn solana_sdk::process_instruction::InvokeContext| {
                $crate::builtin_process_instruction(
                    $process_instruction,
                    first_instruction_account,
                    input,
                    invoke_context,
                )
            },
        )
    };
}

fn get_sysvar<T: Default + Sysvar + Sized + serde::de::DeserializeOwned>(
    id: &Pubkey,
    var_addr: *mut u8,
) -> u64 {
    let invoke_context = get_invoke_context();
    if invoke_context
        .get_compute_meter()
        .try_borrow_mut()
        .map_err(|_| ACCOUNT_BORROW_FAILED)
        .unwrap()
        .consume(invoke_context.get_compute_budget().sysvar_base_cost + T::size_of() as u64)
        .is_err()
    {
        panic!("Exceeded compute budget");
    }

    match solana_program_runtime::invoke_context::get_sysvar::<T>(invoke_context, id) {
        Ok(sysvar_data) => unsafe {
            *(var_addr as *mut _ as *mut T) = sysvar_data;
            SUCCESS
        },
        Err(_) => UNSUPPORTED_SYSVAR,
    }
}

struct SyscallStubs {}
impl solana_sdk::program_stubs::SyscallStubs for SyscallStubs {
    fn sol_log(&self, message: &str) {
        let invoke_context = get_invoke_context();
        let logger = invoke_context.get_logger();
        let logger = logger.borrow_mut();
        if logger.log_enabled() {
            logger.log(&format!("Program log: {}", message));
        }
    }

    fn sol_invoke_signed(
        &self,
        instruction: &Instruction,
        account_infos: &[AccountInfo],
        signers_seeds: &[&[&[u8]]],
    ) -> ProgramResult {
        //
        // TODO: Merge the business logic below with the BPF invoke path in
        //       programs/bpf_loader/src/syscalls.rs
        //

        let invoke_context = get_invoke_context();
        let logger = invoke_context.get_logger();

        let caller = *invoke_context.get_caller().expect("get_caller");
        let message = Message::new(&[instruction.clone()], None);
        let program_id_index = message.instructions[0].program_id_index as usize;
        let program_id = message.account_keys[program_id_index];
        let demote_program_write_locks =
            invoke_context.is_feature_active(&demote_program_write_locks::id());
        // TODO don't have the caller's keyed_accounts so can't validate writer or signer escalation or deescalation yet
        let caller_privileges = message
            .account_keys
            .iter()
            .enumerate()
            .map(|(i, _)| message.is_writable(i, demote_program_write_locks))
            .collect::<Vec<bool>>();

        stable_log::program_invoke(&logger, &program_id, invoke_context.invoke_depth());

        // Convert AccountInfos into Accounts
        let mut account_indices = Vec::with_capacity(message.account_keys.len());
        let mut accounts = Vec::with_capacity(message.account_keys.len());
        for (i, account_key) in message.account_keys.iter().enumerate() {
            let ((account_index, account), account_info) = invoke_context
                .get_account(account_key)
                .zip(
                    account_infos
                        .iter()
                        .find(|account_info| account_info.unsigned_key() == account_key),
                )
                .ok_or(InstructionError::MissingAccount)
                .unwrap();
            {
                let mut account = account.borrow_mut();
                account.copy_into_owner_from_slice(account_info.owner.as_ref());
                account.set_data_from_slice(&account_info.try_borrow_data().unwrap());
                account.set_lamports(account_info.lamports());
                account.set_executable(account_info.executable);
                account.set_rent_epoch(account_info.rent_epoch);
            }
            let account_info = if message.is_writable(i, demote_program_write_locks) {
                Some(account_info)
            } else {
                None
            };
            account_indices.push(account_index);
            accounts.push((account, account_info));
        }
        let (program_account_index, _program_account) =
            invoke_context.get_account(&program_id).unwrap();
        let program_indices = vec![program_account_index];

        // Check Signers
        for account_info in account_infos {
            for instruction_account in &instruction.accounts {
                if *account_info.unsigned_key() == instruction_account.pubkey
                    && instruction_account.is_signer
                    && !account_info.is_signer
                {
                    let mut program_signer = false;
                    for seeds in signers_seeds.iter() {
                        let signer = Pubkey::create_program_address(seeds, &caller).unwrap();
                        if instruction_account.pubkey == signer {
                            program_signer = true;
                            break;
                        }
                    }
                    assert!(
                        program_signer,
                        "Missing signer for {}",
                        instruction_account.pubkey
                    );
                }
            }
        }

        invoke_context.record_instruction(instruction);

        InstructionProcessor::process_cross_program_instruction(
            &message,
            &program_indices,
            &account_indices,
            &caller_privileges,
            invoke_context,
        )
        .map_err(|err| ProgramError::try_from(err).unwrap_or_else(|err| panic!("{}", err)))?;

        // Copy writeable account modifications back into the caller's AccountInfos
        for (account, account_info) in accounts.iter() {
            if let Some(account_info) = account_info {
                **account_info.try_borrow_mut_lamports().unwrap() = account.borrow().lamports();
                let mut data = account_info.try_borrow_mut_data()?;
                let account_borrow = account.borrow();
                let new_data = account_borrow.data();
                if account_info.owner != account.borrow().owner() {
                    // TODO Figure out a better way to allow the System Program to set the account owner
                    #[allow(clippy::transmute_ptr_to_ptr)]
                    #[allow(mutable_transmutes)]
                    let account_info_mut =
                        unsafe { transmute::<&Pubkey, &mut Pubkey>(account_info.owner) };
                    *account_info_mut = *account.borrow().owner();
                }
                // TODO: Figure out how to allow the System Program to resize the account data
                assert!(
                    data.len() == new_data.len(),
                    "Account data resizing not supported yet: {} -> {}. \
                        Consider making this test conditional on `#[cfg(feature = \"test-bpf\")]`",
                    data.len(),
                    new_data.len()
                );
                data.clone_from_slice(new_data);
            }
        }

        stable_log::program_success(&logger, &program_id);
        Ok(())
    }

    fn sol_get_clock_sysvar(&self, var_addr: *mut u8) -> u64 {
        get_sysvar::<Clock>(&clock::id(), var_addr)
    }

    fn sol_get_epoch_schedule_sysvar(&self, var_addr: *mut u8) -> u64 {
        get_sysvar::<EpochSchedule>(&epoch_schedule::id(), var_addr)
    }

    #[allow(deprecated)]
    fn sol_get_fees_sysvar(&self, var_addr: *mut u8) -> u64 {
        get_sysvar::<Fees>(&fees::id(), var_addr)
    }

    fn sol_get_rent_sysvar(&self, var_addr: *mut u8) -> u64 {
        get_sysvar::<Rent>(&rent::id(), var_addr)
    }
}

pub fn find_file(filename: &str) -> Option<PathBuf> {
    for dir in default_shared_object_dirs() {
        let candidate = dir.join(&filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn default_shared_object_dirs() -> Vec<PathBuf> {
    let mut search_path = vec![];
    if let Ok(bpf_out_dir) = std::env::var("BPF_OUT_DIR") {
        search_path.push(PathBuf::from(bpf_out_dir));
    }
    search_path.push(PathBuf::from("tests/fixtures"));
    if let Ok(dir) = std::env::current_dir() {
        search_path.push(dir);
    }
    trace!("BPF .so search path: {:?}", search_path);
    search_path
}

pub fn read_file<P: AsRef<Path>>(path: P) -> Vec<u8> {
    let path = path.as_ref();
    let mut file = File::open(path)
        .unwrap_or_else(|err| panic!("Failed to open \"{}\": {}", path.display(), err));

    let mut file_data = Vec::new();
    file.read_to_end(&mut file_data)
        .unwrap_or_else(|err| panic!("Failed to read \"{}\": {}", path.display(), err));
    file_data
}

fn setup_fees(bank: Bank) -> Bank {
    // Realistic fees part 1: Fake a single signature by calling
    // `bank.commit_transactions()` so that the fee in the child bank will be
    // initialized with a non-zero fee.
    assert_eq!(bank.signature_count(), 0);
    bank.commit_transactions(
        &[],     // transactions
        &mut [], // loaded accounts
        &[],     // transaction execution results
        0,       // tx count
        1,       // signature count
        &mut ExecuteTimings::default(),
    );
    assert_eq!(bank.signature_count(), 1);

    // Advance beyond slot 0 for a slightly more realistic test environment
    let bank = Arc::new(bank);
    let bank = Bank::new_from_parent(&bank, bank.collector_id(), bank.slot() + 1);
    debug!("Bank slot: {}", bank.slot());

    // Realistic fees part 2: Tick until a new blockhash is produced to pick up the
    // non-zero fees
    let last_blockhash = bank.last_blockhash();
    while last_blockhash == bank.last_blockhash() {
        bank.register_tick(&Hash::new_unique());
    }

    // Make sure a fee is now required
    let lamports_per_signature = bank.get_lamports_per_signature();
    assert_ne!(lamports_per_signature, 0);

    bank
}

pub struct ProgramTest {
    accounts: Vec<(Pubkey, AccountSharedData)>,
    builtins: Vec<Builtin>,
    compute_max_units: Option<u64>,
    prefer_bpf: bool,
    use_bpf_jit: bool,
}

impl Default for ProgramTest {
    /// Initialize a new ProgramTest
    ///
    /// If the `BPF_OUT_DIR` environment variable is defined, BPF programs will be preferred over
    /// over a native instruction processor.  The `ProgramTest::prefer_bpf()` method may be
    /// used to override this preference at runtime.  `cargo test-bpf` will set `BPF_OUT_DIR`
    /// automatically.
    ///
    /// BPF program shared objects and account data files are searched for in
    /// * the value of the `BPF_OUT_DIR` environment variable
    /// * the `tests/fixtures` sub-directory
    /// * the current working directory
    ///
    fn default() -> Self {
        solana_logger::setup_with_default(
            "solana_rbpf::vm=debug,\
             solana_runtime::message_processor=debug,\
             solana_runtime::system_instruction_processor=trace,\
             solana_program_test=info",
        );
        let prefer_bpf = std::env::var("BPF_OUT_DIR").is_ok();

        Self {
            accounts: vec![],
            builtins: vec![],
            compute_max_units: None,
            prefer_bpf,
            use_bpf_jit: false,
        }
    }
}

impl ProgramTest {
    /// Create a `ProgramTest`.
    ///
    /// This is a wrapper around [`default`] and [`add_program`]. See their documentation for more
    /// details.
    ///
    /// [`default`]: #method.default
    /// [`add_program`]: #method.add_program
    pub fn new(
        program_name: &str,
        program_id: Pubkey,
        process_instruction: Option<ProcessInstructionWithContext>,
    ) -> Self {
        let mut me = Self::default();
        me.add_program(program_name, program_id, process_instruction);
        me
    }

    /// Override default BPF program selection
    pub fn prefer_bpf(&mut self, prefer_bpf: bool) {
        self.prefer_bpf = prefer_bpf;
    }

    /// Override the default maximum compute units
    pub fn set_compute_max_units(&mut self, compute_max_units: u64) {
        self.compute_max_units = Some(compute_max_units);
    }

    /// Override the BPF compute budget
    #[allow(deprecated)]
    #[deprecated(since = "1.8.0", note = "please use `set_compute_max_units` instead")]
    pub fn set_bpf_compute_max_units(&mut self, bpf_compute_max_units: u64) {
        self.compute_max_units = Some(bpf_compute_max_units);
    }

    /// Execute the BPF program with JIT if true, interpreted if false
    pub fn use_bpf_jit(&mut self, use_bpf_jit: bool) {
        self.use_bpf_jit = use_bpf_jit;
    }

    /// Add an account to the test environment
    pub fn add_account(&mut self, address: Pubkey, account: Account) {
        self.accounts
            .push((address, AccountSharedData::from(account)));
    }

    /// Add an account to the test environment with the account data in the provided `filename`
    pub fn add_account_with_file_data(
        &mut self,
        address: Pubkey,
        lamports: u64,
        owner: Pubkey,
        filename: &str,
    ) {
        self.add_account(
            address,
            Account {
                lamports,
                data: read_file(find_file(filename).unwrap_or_else(|| {
                    panic!("Unable to locate {}", filename);
                })),
                owner,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    /// Add an account to the test environment with the account data in the provided as a base 64
    /// string
    pub fn add_account_with_base64_data(
        &mut self,
        address: Pubkey,
        lamports: u64,
        owner: Pubkey,
        data_base64: &str,
    ) {
        self.add_account(
            address,
            Account {
                lamports,
                data: base64::decode(data_base64)
                    .unwrap_or_else(|err| panic!("Failed to base64 decode: {}", err)),
                owner,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    /// Add a BPF program to the test environment.
    ///
    /// `program_name` will also be used to locate the BPF shared object in the current or fixtures
    /// directory.
    ///
    /// If `process_instruction` is provided, the natively built-program may be used instead of the
    /// BPF shared object depending on the `BPF_OUT_DIR` environment variable.
    pub fn add_program(
        &mut self,
        program_name: &str,
        program_id: Pubkey,
        process_instruction: Option<ProcessInstructionWithContext>,
    ) {
        let add_bpf = |this: &mut ProgramTest, program_file: PathBuf| {
            let data = read_file(&program_file);
            info!(
                "\"{}\" BPF program from {}{}",
                program_name,
                program_file.display(),
                std::fs::metadata(&program_file)
                    .map(|metadata| {
                        metadata
                            .modified()
                            .map(|time| {
                                format!(
                                    ", modified {}",
                                    HumanTime::from(time)
                                        .to_text_en(Accuracy::Precise, Tense::Past)
                                )
                            })
                            .ok()
                    })
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "".to_string())
            );

            this.add_account(
                program_id,
                Account {
                    lamports: Rent::default().minimum_balance(data.len()).min(1),
                    data,
                    owner: solana_sdk::bpf_loader::id(),
                    executable: true,
                    rent_epoch: 0,
                },
            );
        };

        let add_native = |this: &mut ProgramTest, process_fn: ProcessInstructionWithContext| {
            info!("\"{}\" program loaded as native code", program_name);
            this.builtins
                .push(Builtin::new(program_name, program_id, process_fn));
        };

        let warn_invalid_program_name = || {
            let valid_program_names = default_shared_object_dirs()
                .iter()
                .filter_map(|dir| dir.read_dir().ok())
                .flat_map(|read_dir| {
                    read_dir.filter_map(|entry| {
                        let path = entry.ok()?.path();
                        if !path.is_file() {
                            return None;
                        }
                        match path.extension()?.to_str()? {
                            "so" => Some(path.file_stem()?.to_os_string()),
                            _ => None,
                        }
                    })
                })
                .collect::<Vec<_>>();

            if valid_program_names.is_empty() {
                // This should be unreachable as `test-bpf` should guarantee at least one shared
                // object exists somewhere.
                warn!("No BPF shared objects found.");
                return;
            }

            warn!(
                "Possible bogus program name. Ensure the program name ({}) \
                matches one of the following recognizable program names:",
                program_name,
            );
            for name in valid_program_names {
                warn!(" - {}", name.to_str().unwrap());
            }
        };

        let program_file = find_file(&format!("{}.so", program_name));
        match (self.prefer_bpf, program_file, process_instruction) {
            // If BPF is preferred (i.e., `test-bpf` is invoked) and a BPF shared object exists,
            // use that as the program data.
            (true, Some(file), _) => add_bpf(self, file),

            // If BPF is not required (i.e., we were invoked with `test`), use the provided
            // processor function as is.
            //
            // TODO: figure out why tests hang if a processor panics when running native code.
            (false, _, Some(process)) => add_native(self, process),

            // Invalid: `test-bpf` invocation with no matching BPF shared object.
            (true, None, _) => {
                warn_invalid_program_name();
                panic!(
                    "Program file data not available for {} ({})",
                    program_name, program_id
                );
            }

            // Invalid: regular `test` invocation without a processor.
            (false, _, None) => {
                panic!(
                    "Program processor not available for {} ({})",
                    program_name, program_id
                );
            }
        }
    }

    /// Add a builtin program to the test environment.
    ///
    /// Note that builtin programs are responsible for their own `stable_log` output.
    pub fn add_builtin_program(
        &mut self,
        program_name: &str,
        program_id: Pubkey,
        process_instruction: ProcessInstructionWithContext,
    ) {
        info!("\"{}\" builtin program", program_name);
        self.builtins
            .push(Builtin::new(program_name, program_id, process_instruction));
    }

    fn setup_bank(
        &self,
    ) -> (
        Arc<RwLock<BankForks>>,
        Arc<RwLock<BlockCommitmentCache>>,
        Hash,
        GenesisConfigInfo,
    ) {
        {
            use std::sync::Once;
            static ONCE: Once = Once::new();

            ONCE.call_once(|| {
                solana_sdk::program_stubs::set_syscall_stubs(Box::new(SyscallStubs {}));
            });
        }

        let rent = Rent::default();
        let fee_rate_governor = FeeRateGovernor::default();
        let bootstrap_validator_pubkey = Pubkey::new_unique();
        let bootstrap_validator_stake_lamports =
            rent.minimum_balance(VoteState::size_of()) + sol_to_lamports(1_000_000.0);

        let mint_keypair = Keypair::new();
        let voting_keypair = Keypair::new();

        let mut genesis_config = create_genesis_config_with_leader_ex(
            sol_to_lamports(1_000_000.0),
            &mint_keypair.pubkey(),
            &bootstrap_validator_pubkey,
            &voting_keypair.pubkey(),
            &Pubkey::new_unique(),
            bootstrap_validator_stake_lamports,
            42,
            fee_rate_governor,
            rent,
            ClusterType::Development,
            vec![],
        );
        let target_tick_duration = Duration::from_micros(100);
        genesis_config.poh_config = PohConfig::new_sleep(target_tick_duration);
        debug!("Payer address: {}", mint_keypair.pubkey());
        debug!("Genesis config: {}", genesis_config);

        let mut bank = Bank::new_for_tests(&genesis_config);

        // Add loaders
        macro_rules! add_builtin {
            ($b:expr) => {
                bank.add_builtin(&$b.0, &$b.1, $b.2)
            };
        }
        add_builtin!(solana_bpf_loader_deprecated_program!());
        if self.use_bpf_jit {
            add_builtin!(solana_bpf_loader_program_with_jit!());
            add_builtin!(solana_bpf_loader_upgradeable_program_with_jit!());
        } else {
            add_builtin!(solana_bpf_loader_program!());
            add_builtin!(solana_bpf_loader_upgradeable_program!());
        }

        // Add commonly-used SPL programs as a convenience to the user
        for (program_id, account) in programs::spl_programs(&Rent::default()).iter() {
            bank.store_account(program_id, account);
        }

        // User-supplied additional builtins
        for builtin in self.builtins.iter() {
            bank.add_builtin(
                &builtin.name,
                &builtin.id,
                builtin.process_instruction_with_context,
            );
        }

        for (address, account) in self.accounts.iter() {
            if bank.get_account(address).is_some() {
                info!("Overriding account at {}", address);
            }
            bank.store_account(address, account);
        }
        bank.set_capitalization();
        if let Some(max_units) = self.compute_max_units {
            bank.set_compute_budget(Some(ComputeBudget {
                max_units,
                ..ComputeBudget::default()
            }));
        }
        let bank = setup_fees(bank);
        let slot = bank.slot();
        let last_blockhash = bank.last_blockhash();
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let block_commitment_cache = Arc::new(RwLock::new(
            BlockCommitmentCache::new_for_tests_with_slots(slot, slot),
        ));

        (
            bank_forks,
            block_commitment_cache,
            last_blockhash,
            GenesisConfigInfo {
                genesis_config,
                mint_keypair,
                voting_keypair,
            },
        )
    }

    pub async fn start(self) -> (BanksClient, Keypair, Hash) {
        let (bank_forks, block_commitment_cache, last_blockhash, gci) = self.setup_bank();
        let target_tick_duration = gci.genesis_config.poh_config.target_tick_duration;
        let transport = start_local_server(
            bank_forks.clone(),
            block_commitment_cache.clone(),
            target_tick_duration,
        )
        .await;
        let banks_client = start_client(transport)
            .await
            .unwrap_or_else(|err| panic!("Failed to start banks client: {}", err));

        // Run a simulated PohService to provide the client with new blockhashes.  New blockhashes
        // are required when sending multiple otherwise identical transactions in series from a
        // test
        tokio::spawn(async move {
            loop {
                bank_forks
                    .read()
                    .unwrap()
                    .working_bank()
                    .register_tick(&Hash::new_unique());
                tokio::time::sleep(target_tick_duration).await;
            }
        });

        (banks_client, gci.mint_keypair, last_blockhash)
    }

    /// Start the test client
    ///
    /// Returns a `BanksClient` interface into the test environment as well as a payer `Keypair`
    /// with SOL for sending transactions
    pub async fn start_with_context(self) -> ProgramTestContext {
        let (bank_forks, block_commitment_cache, last_blockhash, gci) = self.setup_bank();
        let target_tick_duration = gci.genesis_config.poh_config.target_tick_duration;
        let transport = start_local_server(
            bank_forks.clone(),
            block_commitment_cache.clone(),
            target_tick_duration,
        )
        .await;
        let banks_client = start_client(transport)
            .await
            .unwrap_or_else(|err| panic!("Failed to start banks client: {}", err));

        ProgramTestContext::new(
            bank_forks,
            block_commitment_cache,
            banks_client,
            last_blockhash,
            gci,
        )
    }
}

#[async_trait]
pub trait ProgramTestBanksClientExt {
    /// Get a new blockhash, similar in spirit to RpcClient::get_new_blockhash()
    ///
    /// This probably should eventually be moved into BanksClient proper in some form
    #[deprecated(
        since = "1.9.0",
        note = "Please use `get_new_latest_blockhash `instead"
    )]
    async fn get_new_blockhash(&mut self, blockhash: &Hash) -> io::Result<(Hash, FeeCalculator)>;
    /// Get a new latest blockhash, similar in spirit to RpcClient::get_latest_blockhash()
    async fn get_new_latest_blockhash(&mut self, blockhash: &Hash) -> io::Result<Hash>;
}

#[async_trait]
impl ProgramTestBanksClientExt for BanksClient {
    async fn get_new_blockhash(&mut self, blockhash: &Hash) -> io::Result<(Hash, FeeCalculator)> {
        let mut num_retries = 0;
        let start = Instant::now();
        while start.elapsed().as_secs() < 5 {
            #[allow(deprecated)]
            if let Ok((fee_calculator, new_blockhash, _slot)) = self.get_fees().await {
                if new_blockhash != *blockhash {
                    return Ok((new_blockhash, fee_calculator));
                }
            }
            debug!("Got same blockhash ({:?}), will retry...", blockhash);

            tokio::time::sleep(Duration::from_millis(200)).await;
            num_retries += 1;
        }

        Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "Unable to get new blockhash after {}ms (retried {} times), stuck at {}",
                start.elapsed().as_millis(),
                num_retries,
                blockhash
            ),
        ))
    }

    async fn get_new_latest_blockhash(&mut self, blockhash: &Hash) -> io::Result<Hash> {
        let mut num_retries = 0;
        let start = Instant::now();
        while start.elapsed().as_secs() < 5 {
            let new_blockhash = self.get_latest_blockhash().await?;
            if new_blockhash != *blockhash {
                return Ok(new_blockhash);
            }
            debug!("Got same blockhash ({:?}), will retry...", blockhash);

            tokio::time::sleep(Duration::from_millis(200)).await;
            num_retries += 1;
        }

        Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "Unable to get new blockhash after {}ms (retried {} times), stuck at {}",
                start.elapsed().as_millis(),
                num_retries,
                blockhash
            ),
        ))
    }
}

struct DroppableTask<T>(Arc<AtomicBool>, JoinHandle<T>);

impl<T> Drop for DroppableTask<T> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub struct ProgramTestContext {
    pub banks_client: BanksClient,
    pub last_blockhash: Hash,
    pub payer: Keypair,
    genesis_config: GenesisConfig,
    bank_forks: Arc<RwLock<BankForks>>,
    block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
    _bank_task: DroppableTask<()>,
}

impl ProgramTestContext {
    fn new(
        bank_forks: Arc<RwLock<BankForks>>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        banks_client: BanksClient,
        last_blockhash: Hash,
        genesis_config_info: GenesisConfigInfo,
    ) -> Self {
        // Run a simulated PohService to provide the client with new blockhashes.  New blockhashes
        // are required when sending multiple otherwise identical transactions in series from a
        // test
        let running_bank_forks = bank_forks.clone();
        let target_tick_duration = genesis_config_info
            .genesis_config
            .poh_config
            .target_tick_duration;
        let exit = Arc::new(AtomicBool::new(false));
        let bank_task = DroppableTask(
            exit.clone(),
            tokio::spawn(async move {
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }
                    running_bank_forks
                        .read()
                        .unwrap()
                        .working_bank()
                        .register_tick(&Hash::new_unique());
                    tokio::time::sleep(target_tick_duration).await;
                }
            }),
        );

        Self {
            banks_client,
            last_blockhash,
            payer: genesis_config_info.mint_keypair,
            genesis_config: genesis_config_info.genesis_config,
            bank_forks,
            block_commitment_cache,
            _bank_task: bank_task,
        }
    }

    pub fn genesis_config(&self) -> &GenesisConfig {
        &self.genesis_config
    }

    /// Manually increment vote credits for the current epoch in the specified vote account to simulate validator voting activity
    pub fn increment_vote_account_credits(
        &mut self,
        vote_account_address: &Pubkey,
        number_of_credits: u64,
    ) {
        let bank_forks = self.bank_forks.read().unwrap();
        let bank = bank_forks.working_bank();

        // generate some vote activity for rewards
        let mut vote_account = bank.get_account(vote_account_address).unwrap();
        let mut vote_state = VoteState::from(&vote_account).unwrap();

        let epoch = bank.epoch();
        for _ in 0..number_of_credits {
            vote_state.increment_credits(epoch);
        }
        let versioned = VoteStateVersions::new_current(vote_state);
        VoteState::to(&versioned, &mut vote_account).unwrap();
        bank.store_account(vote_account_address, &vote_account);
    }

    /// Force the working bank ahead to a new slot
    pub fn warp_to_slot(&mut self, warp_slot: Slot) -> Result<(), ProgramTestError> {
        let mut bank_forks = self.bank_forks.write().unwrap();
        let bank = bank_forks.working_bank();

        // Force ticks until a new blockhash, otherwise retried transactions will have
        // the same signature
        let last_blockhash = bank.last_blockhash();
        while last_blockhash == bank.last_blockhash() {
            bank.register_tick(&Hash::new_unique());
        }

        // warp ahead to one slot *before* the desired slot because the warped
        // bank is frozen
        let working_slot = bank.slot();
        if warp_slot <= working_slot {
            return Err(ProgramTestError::InvalidWarpSlot);
        }

        let pre_warp_slot = warp_slot - 1;
        let warp_bank = bank_forks.insert(Bank::warp_from_parent(
            &bank,
            &Pubkey::default(),
            pre_warp_slot,
        ));
        bank_forks.set_root(
            pre_warp_slot,
            &solana_runtime::accounts_background_service::AbsRequestSender::default(),
            Some(pre_warp_slot),
        );

        // warp bank is frozen, so go forward one slot from it
        bank_forks.insert(Bank::new_from_parent(
            &warp_bank,
            &Pubkey::default(),
            warp_slot,
        ));

        // Update block commitment cache, otherwise banks server will poll at
        // the wrong slot
        let mut w_block_commitment_cache = self.block_commitment_cache.write().unwrap();
        // HACK: The root set here should be `pre_warp_slot`, but since we're
        // in a testing environment, the root bank never updates after a warp.
        // The ticking thread only updates the working bank, and never the root
        // bank.
        w_block_commitment_cache.set_all_slots(warp_slot, warp_slot);

        let bank = bank_forks.working_bank();
        self.last_blockhash = bank.last_blockhash();
        Ok(())
    }
}
