//
// Copyright 2023 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::marker::PhantomData;
use std::time::{Duration, SystemTime};

use attest::svr2::RaftConfig;
use attest::{cds2, enclave, nitro};
use derive_where::derive_where;
use http::uri::PathAndQuery;

use crate::env::{DomainConfig, Svr3Env};
use crate::infra::connection_manager::{
    MultiRouteConnectionManager, SingleRouteThrottlingConnectionManager,
};
use crate::infra::ws::AttestedConnection;
use crate::infra::{make_ws_config, ConnectionParams, EndpointConnection};
use crate::svr::SvrConnection;

pub trait EnclaveKind {
    fn url_path(enclave: &[u8]) -> PathAndQuery;
}
pub trait Svr3Flavor: EnclaveKind {}

pub enum Cdsi {}

pub enum Sgx {}

pub enum Nitro {}

impl EnclaveKind for Cdsi {
    fn url_path(enclave: &[u8]) -> PathAndQuery {
        PathAndQuery::try_from(format!("/v1/{}/discovery", hex::encode(enclave))).unwrap()
    }
}

impl EnclaveKind for Sgx {
    fn url_path(enclave: &[u8]) -> PathAndQuery {
        PathAndQuery::try_from(format!("/v1/{}", hex::encode(enclave))).unwrap()
    }
}

impl EnclaveKind for Nitro {
    fn url_path(enclave: &[u8]) -> PathAndQuery {
        PathAndQuery::try_from(format!(
            "/v1/{}",
            std::str::from_utf8(enclave).expect("valid utf8")
        ))
        .unwrap()
    }
}

impl Svr3Flavor for Sgx {}

impl Svr3Flavor for Nitro {}

pub trait IntoConnections {
    type Connections: ArrayIsh<AttestedConnection> + Send;
    fn into_connections(self) -> Self::Connections;
}

impl<A> IntoConnections for A
where
    A: Into<AttestedConnection>,
{
    type Connections = [AttestedConnection; 1];
    fn into_connections(self) -> Self::Connections {
        [self.into()]
    }
}

impl<A, B> IntoConnections for (A, B)
where
    A: Into<AttestedConnection>,
    B: Into<AttestedConnection>,
{
    type Connections = [AttestedConnection; 2];
    fn into_connections(self) -> Self::Connections {
        [self.0.into(), self.1.into()]
    }
}

impl<A, B, C> IntoConnections for (A, B, C)
where
    A: Into<AttestedConnection>,
    B: Into<AttestedConnection>,
    C: Into<AttestedConnection>,
{
    type Connections = [AttestedConnection; 3];
    fn into_connections(self) -> Self::Connections {
        [self.0.into(), self.1.into(), self.2.into()]
    }
}

pub trait ArrayIsh<T>: AsRef<[T]> + AsMut<[T]> {
    const N: usize;
}

impl<T, const N: usize> ArrayIsh<T> for [T; N] {
    const N: usize = N;
}

pub trait PpssSetup {
    type Connections: IntoConnections + Send;
    type ServerIds: ArrayIsh<u64> + Send;
    const N: usize = Self::ServerIds::N;
    fn server_ids() -> Self::ServerIds;
}

impl PpssSetup for Svr3Env<'_> {
    type Connections = (SvrConnection<Sgx>, SvrConnection<Nitro>);
    type ServerIds = [u64; 2];

    fn server_ids() -> Self::ServerIds {
        [1, 2]
    }
}

#[derive_where(Clone, Copy; Bytes)]
pub struct MrEnclave<Bytes, E> {
    inner: Bytes,
    enclave_kind: PhantomData<E>,
}

impl<Bytes, E: EnclaveKind> MrEnclave<Bytes, E> {
    pub const fn new(bytes: Bytes) -> Self {
        Self {
            inner: bytes,
            enclave_kind: PhantomData,
        }
    }
}

impl<Bytes: AsRef<[u8]>, S> AsRef<[u8]> for MrEnclave<Bytes, S> {
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

#[derive_where(Copy, Clone)]
pub struct EnclaveEndpoint<'a, E: EnclaveKind> {
    pub domain_config: DomainConfig,
    pub mr_enclave: MrEnclave<&'a [u8], E>,
}

pub trait NewHandshake {
    fn new_handshake(
        params: &EndpointParams<Self>,
        attestation_message: &[u8],
    ) -> enclave::Result<enclave::Handshake>
    where
        Self: EnclaveKind + Sized;
}

pub struct EndpointParams<E: EnclaveKind> {
    pub(crate) mr_enclave: MrEnclave<&'static [u8], E>,
    pub(crate) raft_config_override: Option<&'static RaftConfig>,
}

impl<E: EnclaveKind> EndpointParams<E> {
    pub const fn new(mr_enclave: MrEnclave<&'static [u8], E>) -> Self {
        Self {
            mr_enclave,
            raft_config_override: None,
        }
    }

    pub fn with_raft_override(mut self, raft_config: &'static RaftConfig) -> Self {
        self.raft_config_override = Some(raft_config);
        self
    }
}

pub struct EnclaveEndpointConnection<E: EnclaveKind, C> {
    pub(crate) endpoint_connection: EndpointConnection<C>,
    pub(crate) params: EndpointParams<E>,
}

impl<E: EnclaveKind> EnclaveEndpointConnection<E, SingleRouteThrottlingConnectionManager> {
    pub fn new(endpoint: EnclaveEndpoint<'static, E>, connect_timeout: Duration) -> Self {
        Self::with_custom_properties(endpoint, connect_timeout, None)
    }

    pub fn with_custom_properties(
        endpoint: EnclaveEndpoint<'static, E>,
        connect_timeout: Duration,
        raft_config_override: Option<&'static RaftConfig>,
    ) -> Self {
        Self {
            endpoint_connection: EndpointConnection {
                manager: SingleRouteThrottlingConnectionManager::new(
                    endpoint.domain_config.connection_params(),
                    connect_timeout,
                ),
                config: make_ws_config(E::url_path(endpoint.mr_enclave.as_ref()), connect_timeout),
            },
            params: EndpointParams {
                mr_enclave: endpoint.mr_enclave,
                raft_config_override,
            },
        }
    }
}

impl<E: EnclaveKind> EnclaveEndpointConnection<E, MultiRouteConnectionManager> {
    pub fn new_multi(
        mr_enclave: MrEnclave<&'static [u8], E>,
        connection_params: impl IntoIterator<Item = ConnectionParams>,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            endpoint_connection: EndpointConnection::new_multi(
                connection_params,
                connect_timeout,
                make_ws_config(E::url_path(mr_enclave.as_ref()), connect_timeout),
            ),
            params: EndpointParams {
                mr_enclave,
                raft_config_override: None,
            },
        }
    }
}

impl NewHandshake for Sgx {
    fn new_handshake(
        params: &EndpointParams<Self>,
        attestation_message: &[u8],
    ) -> enclave::Result<enclave::Handshake> {
        attest::svr2::new_handshake_with_override(
            params.mr_enclave.as_ref(),
            attestation_message,
            SystemTime::now(),
            params.raft_config_override,
        )
    }
}

impl NewHandshake for Cdsi {
    fn new_handshake(
        params: &EndpointParams<Self>,
        attestation_message: &[u8],
    ) -> enclave::Result<enclave::Handshake> {
        cds2::new_handshake(
            params.mr_enclave.as_ref(),
            attestation_message,
            SystemTime::now(),
        )
    }
}

impl NewHandshake for Nitro {
    fn new_handshake(
        params: &EndpointParams<Self>,
        attestation_message: &[u8],
    ) -> enclave::Result<enclave::Handshake> {
        nitro::new_handshake(
            params.mr_enclave.as_ref(),
            attestation_message,
            SystemTime::now(),
            params.raft_config_override,
        )
    }
}
