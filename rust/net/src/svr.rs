//
// Copyright 2023 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::marker::PhantomData;

use thiserror::Error;

use crate::auth::HttpBasicAuth;
use crate::enclave::{EnclaveEndpointConnection, NewHandshake, Svr3Flavor};
use crate::infra::connection_manager::ConnectionManager;
use crate::infra::errors::{LogSafeDisplay, NetError};
use crate::infra::reconnect::{ServiceConnectorWithDecorator, ServiceInitializer, ServiceState};
use crate::infra::ws::{
    AttestedConnection, AttestedConnectionError, DefaultStream, WebSocketClientConnector,
};
use crate::infra::{AsyncDuplexStream, TransportConnector};

#[derive(Debug, Error, displaydoc::Display)]
pub enum Error {
    /// Network error: {0}
    Net(#[from] NetError),
    /// Protocol error after establishing a connection
    Protocol,
    /// Enclave attestation failed: {0}
    AttestationError(attest::enclave::Error),
}

impl LogSafeDisplay for Error {}

impl From<AttestedConnectionError> for Error {
    fn from(value: AttestedConnectionError) -> Self {
        match value {
            AttestedConnectionError::ClientConnection(_) => Self::Protocol,
            AttestedConnectionError::Net(net) => Self::Net(net),
            AttestedConnectionError::Protocol => Self::Protocol,
            AttestedConnectionError::Sgx(err) => Self::AttestationError(err),
        }
    }
}

pub struct SvrConnection<Flavor: Svr3Flavor, S = DefaultStream> {
    inner: AttestedConnection<S>,
    witness: PhantomData<Flavor>,
}

impl<Flavor: Svr3Flavor> From<SvrConnection<Flavor>> for AttestedConnection {
    fn from(conn: SvrConnection<Flavor>) -> Self {
        conn.inner
    }
}

impl<Flavor: Svr3Flavor, S> SvrConnection<Flavor, S> {
    pub fn new(inner: AttestedConnection<S>) -> Self {
        Self {
            inner,
            witness: PhantomData,
        }
    }
}

impl<E: Svr3Flavor, S: AsyncDuplexStream> SvrConnection<E, S>
where
    E: Svr3Flavor + NewHandshake + Sized,
    S: AsyncDuplexStream,
{
    pub async fn connect<C, T>(
        auth: impl HttpBasicAuth,
        connection: &EnclaveEndpointConnection<E, C>,
        transport_connector: T,
    ) -> Result<Self, Error>
    where
        C: ConnectionManager,
        T: TransportConnector<Stream = S>,
    {
        // TODO: This is almost a direct copy of CdsiConnection::connect. They can be unified.
        let auth_decorator = auth.into();
        let websocket_connector = WebSocketClientConnector::new(
            transport_connector,
            connection.endpoint_connection.config.clone(),
        );
        let connector = ServiceConnectorWithDecorator::new(&websocket_connector, auth_decorator);
        let service_initializer =
            ServiceInitializer::new(&connector, &connection.endpoint_connection.manager);
        let connection_attempt_result = service_initializer.connect().await;
        let websocket = match connection_attempt_result {
            ServiceState::Active(websocket, _) => Ok(websocket),
            ServiceState::Cooldown(_) => Err(Error::Net(NetError::NoServiceConnection)),
            ServiceState::Error(e) => Err(Error::Net(e)),
            ServiceState::TimedOut => Err(Error::Net(NetError::Timeout)),
        }?;
        let attested = AttestedConnection::connect(websocket, |attestation_msg| {
            E::new_handshake(&connection.params, attestation_msg)
        })
        .await?;

        Ok(Self::new(attested))
    }
}
