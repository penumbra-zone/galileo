//! RPC facilities.
//!
//! Call [`rpc()`] to obtain a [`Router`] that can handle Galileo RPC calls.

use {
    crate::proto::faucet::v1::{
        faucet_service_server::{FaucetService, FaucetServiceServer},
        FaucetRequest, FaucetResponse,
    },
    anyhow::Context,
    tokio::sync::mpsc,
    tonic::{transport::server::Router, Request, Response, Status},
};

/// Returns a new [`Router`] for handing Galileo RPC calls.
pub fn rpc(request_tx: mpsc::Sender<crate::responder::Request>) -> anyhow::Result<Router> {
    let faucet = FaucetServiceServer::new(FaucetServer::new(request_tx));
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(crate::proto::FILE_DESCRIPTOR_SET)
        .build()
        .context("could not configure grpc reflection service")?;

    Ok(tonic::transport::server::Server::builder()
        .add_service(tonic_web::enable(faucet))
        .add_service(tonic_web::enable(reflection)))
}

/// A faucet server.
///
/// Most importantly, this type implements [`FaucetService`]. This type is generic over an
/// `S` service that provides a [`tower::Service`] implementation from [`SenderRequest`] tuples to
/// the [`TransactionId`] of the transaction that sent funds to the address.
#[derive(Clone)]
struct FaucetServer {
    request_tx: mpsc::Sender<crate::responder::Request>,
}

/// Errors encountered when [`FaucetService::send_funds()`] is called.
enum SendFundsError {
    /// The address was not given.
    AddressMissing,
    /// The address was not valid.
    AddressInvalid(
        <penumbra_keys::Address as TryFrom<penumbra_proto::core::keys::v1::Address>>::Error,
    ),
    /// Failed to send a request to the responder worker.
    SendRequestFailed,
    /// Failed to receive a response from the responder worker.
    ReceiveResponseFailed,
    /// The sender service failed.
    SenderError(String),
}

// === impl FaucetServer ===

impl FaucetServer {
    /// Returns a new faucet server, wrapping the provided sender service.
    fn new(request_tx: mpsc::Sender<crate::responder::Request>) -> Self {
        Self { request_tx }
    }
}

#[tonic::async_trait]
impl FaucetService for FaucetServer {
    /// Sends funds to an address, returning the transaction hash.
    async fn send_funds(
        &self,
        request: Request<FaucetRequest>,
    ) -> Result<Response<FaucetResponse>, Status> {
        let Self { request_tx } = self;

        // Parse the inbound request, getting the address we will send funds to.
        let FaucetRequest { address } = request.into_inner();
        let address: penumbra_keys::Address = address
            .ok_or(SendFundsError::AddressMissing)?
            .try_into()
            .map_err(SendFundsError::AddressInvalid)?;

        // Send the address to the responder worker, which will dispense tokens to the address.
        let (response_rx, request) = crate::responder::Request::new(address.clone());
        request_tx
            .send(request)
            .await
            .map_err(|_| SendFundsError::SendRequestFailed)?;

        // Wait for the responder to send a response back.
        let crate::responder::Response {
            succeeded,
            failed,
            unparsed,
            remaining,
        } = response_rx
            .await
            .map_err(|_| SendFundsError::ReceiveResponseFailed)?;
        assert!(unparsed.is_empty(), "unparsed addresses should be empty");
        assert!(remaining.is_empty(), "no addresses should be remaining");

        let transaction_id = match (succeeded.as_slice(), failed.as_slice()) {
            // We have received the address we sent funds to, and a transaction id.
            ([(address_rx, id)], []) => {
                assert_eq!(
                    address,
                    address_rx.clone(),
                    "responder worker dispensed tokens to the wrong address",
                );
                id.clone()
            }
            // We failed to send funds to this address.
            ([], [(address_rx, error)]) => {
                assert_eq!(
                    address,
                    address_rx.clone(),
                    "responder worker dispensed tokens to the wrong address",
                );
                return Err(error.clone()).map_err(SendFundsError::SenderError).map_err(Status::from);
            }
            _ => panic!("something has gone wrong, responder worker dispensed tokens to unexpected addresses"),
        };

        Ok(FaucetResponse {
            transaction_id: Some(transaction_id.into()),
        })
        .map(Response::new)
    }
}

// === impl SendFundsError ===

impl From<SendFundsError> for Status {
    fn from(error: SendFundsError) -> Status {
        use SendFundsError::*;
        match error {
            AddressMissing => Status::invalid_argument("no address was provided"),
            AddressInvalid(e) => {
                Status::invalid_argument(format!("address could not be parsed: '{e:?}'"))
            }
            SendRequestFailed => Status::unavailable("failed to send request to send worker"),
            SenderError(e) => Status::internal(format!("failed to send funds: '{e:?}'")),
            ReceiveResponseFailed => {
                Status::unavailable("failed to receive response from send worker")
            }
        }
    }
}
