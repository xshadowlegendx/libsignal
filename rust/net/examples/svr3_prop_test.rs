//
// Copyright 2024 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::collections::HashMap;
use std::time::Duration;

use assert_matches::assert_matches;
use libsignal_net::infra::dns::DnsResolver;
use proptest::prelude::*;
use proptest::test_runner::Config;
use proptest_state_machine::{prop_state_machine, ReferenceStateMachine, StateMachineTest};
use rand_core::OsRng;

use libsignal_net::auth::Auth;
use libsignal_net::enclave::{EnclaveEndpointConnection, Nitro, PpssSetup, Sgx};
use libsignal_net::env::Svr3Env;
use libsignal_net::infra::TcpSslTransportConnector;
use libsignal_net::svr::SvrConnection;
use libsignal_net::svr3::{Error, OpaqueMaskedShareSet, PpssOps as _};
use support::*;

const MAX_TRIES_LIMIT: u32 = 10;

// This will result in ~6 requests per minute for each UID. Good enough to avoid throttling
const SLEEP_DURATION: Duration = Duration::from_secs(6);

prop_state_machine! {
    #![proptest_config(Config {
        // Turn failure persistence off for demonstration. This means that no
        // regression file will be captured.
        failure_persistence: None,
        // Enable verbose mode to make the state machine test print the
        // transitions for each case.
        verbose: 1,
        .. Config::default()
    })]
    fn run_test(
            // This is a macro's keyword - only `sequential` is currently supported.
            sequential
            // The number of transitions to be generated for each case. This can
            // be a single numerical value or a range as in here.
            1..20
            // Macro's boilerplate to separate the following identifier.
            =>
            // The name of the type that implements `StateMachineTest`.
            Svr3Storage
        );
}

fn main() {
    init_logger();

    run_test()
}

type Uid = [u8; 16];
type Secret = [u8; 32];

#[derive(Clone, Debug)]
struct Svr3Cell {
    secret: Secret,
    tries_left: u32,
}

impl Svr3Cell {
    pub fn new(secret: Secret, tries_left: u32) -> Self {
        Self { secret, tries_left }
    }
}

#[derive(Clone, Debug)]
pub enum Transition {
    SetUid(Uid),
    Backup(Secret, u32),
    Restore,
    RestoreWithBadPassword,
}

#[derive(Clone, Debug)]
pub struct InMemoryStorage {
    uid: Option<Uid>,
    data: HashMap<Uid, Svr3Cell>,
    last_transition_outcome: TransitionOutcome,
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        InMemoryStorage {
            uid: None,
            data: HashMap::default(),
            last_transition_outcome: TransitionOutcome::Nothing,
        }
    }
}

#[derive(Clone, Debug)]
pub enum TransitionOutcome {
    Nothing,
    NotFound,
    Restored(Secret),
    MaxTriesReached,
    BadCommitment,
}

#[derive(Debug)]
pub struct SUTConfig {
    // Sleep between reconnects to avoid server throttling
    sleep: Option<Duration>,
    // The good client would "forget" the share-set as soon as it cannot restore it anymore.
    forget_share_set: bool,
}

impl Default for SUTConfig {
    fn default() -> Self {
        Self {
            sleep: Some(SLEEP_DURATION),
            forget_share_set: false,
        }
    }
}

pub struct Svr3Storage {
    runtime: tokio::runtime::Runtime,
    env: Svr3Env<'static>,
    current_uid: Option<Uid>,
    sgx_secret: Secret,
    nitro_secret: Secret,
    share_sets: HashMap<Uid, OpaqueMaskedShareSet>,
    config: SUTConfig,
}

impl ReferenceStateMachine for InMemoryStorage {
    type State = Self;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(InMemoryStorage::default()).boxed()
    }

    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
        if state.uid.is_none() {
            return uid().prop_map(Transition::SetUid).boxed();
        }
        // The weights (1, 2 and 3) are to represent that we perform backups twice as often as UID
        // changes, and restores - three times more often.
        prop_oneof![
            1 => uid().prop_map(Transition::SetUid),
            2 => backup_pair().prop_map(|(secret, max_tries)| Transition::Backup(secret, max_tries)),
            3 => Just(Transition::Restore),
            1 => Just(Transition::RestoreWithBadPassword),
        ]
        .boxed()
    }

    fn apply(mut state: Self::State, transition: &Self::Transition) -> Self::State {
        match transition {
            Transition::SetUid(uid) => {
                log::info!("MODEL: set uid to {}", hex::encode(uid));
                state.uid = Some(*uid);
                state.last_transition_outcome = TransitionOutcome::Nothing;
            }
            Transition::Backup(secret, tries_left) => {
                log::info!("MODEL: backup");
                log::debug!("[{}] with {} tries", hex::encode(secret), tries_left);
                let _ = state
                    .data
                    .insert(state.uid.unwrap(), Svr3Cell::new(*secret, *tries_left));
                state.last_transition_outcome = TransitionOutcome::Nothing;
            }
            Transition::Restore | Transition::RestoreWithBadPassword => {
                let expect_bad_commitment =
                    matches!(transition, Transition::RestoreWithBadPassword);
                log::info!("MODEL: restore -> ");
                let uid = state.uid.unwrap();
                let maybe_cell = state.data.get_mut(&uid);
                match maybe_cell {
                    None => {
                        log::info!("\tnot found");
                        state.last_transition_outcome = TransitionOutcome::NotFound;
                    }
                    Some(cell) if cell.tries_left == 0 => {
                        log::info!("\tno more attempts");
                        let _ = state.data.remove(&uid);
                        state.last_transition_outcome = TransitionOutcome::MaxTriesReached;
                    }
                    Some(cell) if expect_bad_commitment => {
                        log::info!("\tbad commitment");
                        cell.tries_left = cell.tries_left.saturating_sub(1);
                        state.last_transition_outcome = TransitionOutcome::BadCommitment;
                    }
                    Some(cell) => {
                        log::info!("\tgood restore");
                        cell.tries_left = cell.tries_left.saturating_sub(1);
                        state.last_transition_outcome = TransitionOutcome::Restored(cell.secret);
                    }
                }
            }
        }
        state
    }
}

impl StateMachineTest for Svr3Storage {
    type SystemUnderTest = Self;
    type Reference = InMemoryStorage;

    fn init_test(
        _ref_state: &<Self::Reference as ReferenceStateMachine>::State,
    ) -> Self::SystemUnderTest {
        Self::new()
    }

    fn apply(
        mut state: Self::SystemUnderTest,
        ref_state: &<Self::Reference as ReferenceStateMachine>::State,
        transition: <Self::Reference as ReferenceStateMachine>::Transition,
    ) -> Self::SystemUnderTest {
        match transition {
            Transition::SetUid(uid) => {
                state.current_uid = Some(uid);
                log::info!("SUT: setting uid");
            }
            Transition::Backup(secret, tries_left) => {
                log::info!("SUT: backup");
                log::debug!("[{}] with {} tries", hex::encode(secret), tries_left);
                let uid = state.current_uid.expect("uid must be set");
                let share_set = state.backup(uid, secret, tries_left);
                let _ = state.share_sets.insert(uid, share_set);
            }
            Transition::Restore | Transition::RestoreWithBadPassword => {
                let expect_bad_commitment =
                    matches!(transition, Transition::RestoreWithBadPassword);
                log::info!("SUT: restore -> ");
                let uid = state.current_uid.expect("uid must be set");
                match state.share_sets.get(&uid) {
                    Some(share_set) => {
                        let password = if expect_bad_commitment {
                            "bad password"
                        } else {
                            "password"
                        };
                        match state.restore(uid, share_set.clone(), password) {
                            Ok(actual_secret) => {
                                assert_matches!(
                                ref_state.last_transition_outcome,
                                TransitionOutcome::Restored(expected_secret) => {
                                    assert_eq!(actual_secret, expected_secret)
                                });
                                log::info!("\tgood restore");
                            }
                            Err(err) => {
                                match err {
                                    Error::DataMissing => {
                                        log::info!("\tvalue missing (no more attempts?)");
                                        // "Forget" the share-set value
                                        // This is what a good client would do.
                                        if state.config.forget_share_set {
                                            let _ = state.share_sets.remove(&uid);
                                        }
                                        assert_matches!(
                                            ref_state.last_transition_outcome,
                                            TransitionOutcome::MaxTriesReached
                                                | TransitionOutcome::NotFound,
                                            "Should have exceeded the tries limit"
                                        );
                                    }
                                    Error::RestoreFailed if expect_bad_commitment => {
                                        log::info!(
                                            "\tbad commitment error (as expected) [{}]",
                                            err
                                        );
                                    }
                                    _ => {
                                        log::info!("\tunexpected svr3 error {}", err);
                                        panic!("unexpected svr3 error {}", err)
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        log::info!("\tnothing to restore");
                        assert_matches!(
                            ref_state.last_transition_outcome,
                            TransitionOutcome::NotFound,
                            "Unexpected not-found"
                        );
                    }
                }
            }
        }
        state
    }
}

impl Svr3Storage {
    fn new() -> Self {
        let sgx_secret = {
            let b64 = std::env::var("SVR3_SGX_SECRET").expect("SGX secret should be set");
            parse_auth_secret(&b64)
        };

        let nitro_secret = {
            let b64 = std::env::var("SVR3_NITRO_SECRET").expect("Nitro secret should be set");
            parse_auth_secret(&b64)
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .expect("can build runtime");
        Self {
            runtime,
            env: libsignal_net::env::STAGING.svr3,
            current_uid: None,
            sgx_secret,
            nitro_secret,
            share_sets: HashMap::default(),
            config: SUTConfig::default(),
        }
    }

    async fn connect(&self, uid: Uid) -> <Svr3Env as PpssSetup>::Connections {
        let connector = TcpSslTransportConnector::new(DnsResolver::default());
        if let Some(duration) = self.config.sleep {
            tokio::time::sleep(duration).await;
        }
        let sgx_connection =
            EnclaveEndpointConnection::new(self.env.sgx(), Duration::from_secs(10));
        let sgx_auth = Auth::from_uid_and_secret(uid, self.sgx_secret);
        let a = SvrConnection::<Sgx>::connect(sgx_auth, &sgx_connection, connector.clone())
            .await
            .expect("can attestedly connect to SGX");

        let nitro_connection =
            EnclaveEndpointConnection::new(self.env.nitro(), Duration::from_secs(10));
        let nitro_auth = Auth::from_uid_and_secret(uid, self.nitro_secret);
        let b = SvrConnection::<Nitro>::connect(nitro_auth, &nitro_connection, connector)
            .await
            .expect("can attestedly connect to Nitro");

        (a, b)
    }

    fn backup(&mut self, uid: Uid, what: Secret, max_tries: u32) -> OpaqueMaskedShareSet {
        self.runtime.block_on(async {
            let mut rng = OsRng;
            let connections = self.connect(uid).await;
            Svr3Env::backup(
                connections,
                "password",
                what,
                max_tries.try_into().unwrap(),
                &mut rng,
            )
            .await
            .expect("can backup")
        })
    }

    fn restore(
        &mut self,
        uid: Uid,
        share_set: OpaqueMaskedShareSet,
        password: &str,
    ) -> Result<[u8; 32], Error> {
        self.runtime.block_on(async {
            let mut rng = OsRng;
            let connections = self.connect(uid).await;
            Svr3Env::restore(connections, password, share_set, &mut rng).await
        })
    }
}

fn uid() -> impl Strategy<Value = Uid> {
    prop_oneof![
        Just([0u8; 16]),
        Just([1u8; 16]),
        Just([2u8; 16]),
        Just([3u8; 16]),
    ]
}

fn secret() -> impl Strategy<Value = Secret> {
    any::<Secret>()
}

fn max_tries() -> impl Strategy<Value = u32> {
    1..MAX_TRIES_LIMIT
}

prop_compose! {
    fn backup_pair()(s in secret(), t in max_tries()) -> (Secret, u32) {
        (s, t)
    }
}

mod support {
    use base64::prelude::{Engine, BASE64_STANDARD};

    pub fn parse_auth_secret(b64: &str) -> [u8; 32] {
        BASE64_STANDARD
            .decode(b64)
            .expect("valid b64")
            .try_into()
            .expect("secret is 32 bytes")
    }

    pub fn init_logger() {
        let _ = env_logger::builder().try_init();
    }
}
