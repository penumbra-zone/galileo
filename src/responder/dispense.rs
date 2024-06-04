//! The core logic for dispensing faucet tokens. See [`dispense()`].

use {
    super::{AddressOrAlmost, Response},
    crate::sender::{Sender, SenderRequest, SenderSet},
    futures_util::{stream, StreamExt as _},
    penumbra_asset::Value,
    penumbra_custody::CustodyClient,
    penumbra_keys::Address,
    penumbra_txhash::TransactionId,
    penumbra_view::ViewClient,
    tower::{
        balance::p2c::Balance, buffer::Buffer, limit::ConcurrencyLimit,
        load::PendingRequestsDiscover, ServiceExt,
    },
};

/// Try to dispense tokens to the given addresses, collecting [`Response`] describing what
/// happened.
pub(super) async fn dispense<V, C>(
    mut addresses: Vec<AddressOrAlmost>,
    max_addresses: usize,
    values: Vec<Value>,
    senders: Buffer<
        ConcurrencyLimit<
            Balance<
                PendingRequestsDiscover<SenderSet<ConcurrencyLimit<Sender<V, C>>>>,
                SenderRequest,
            >,
        >,
        SenderRequest,
    >,
) -> anyhow::Result<Response>
where
    V: ViewClient + Clone + Send + 'static,
    C: CustodyClient + Clone + Send + 'static,
{
    // Track addresses to which we successfully dispensed tokens
    let mut succeeded = Vec::<(Address, TransactionId)>::new();

    // Track addresses (and associated errors) which we tried to send tokens to, but failed
    let mut failed = Vec::<(Address, String)>::new();

    // Track addresses which couldn't be parsed
    let mut unparsed = Vec::<String>::new();

    // Track the requests to send to the sender service.
    let mut reqs = Vec::new();

    // Extract up to the maximum number of permissible valid addresses from the list
    let mut sent_addresses = Vec::new();
    let mut count = 0;
    while count <= max_addresses {
        count += 1;
        match addresses.pop() {
            Some(AddressOrAlmost::Address(addr)) => {
                // Reply to the originating message with the address
                let span = tracing::info_span!("send", address = %addr);
                // TODO: could use tower "load" feature here
                span.in_scope(|| {
                    tracing::info!("processing send request, waiting for readiness");
                });

                let req = (*addr.clone(), values.clone());
                reqs.push(req);
                sent_addresses.push(addr.clone());

                span.in_scope(|| {
                    tracing::info!("submitted send request");
                });
            }
            Some(AddressOrAlmost::Almost(addr)) => {
                unparsed.push(addr);
            }
            None => break,
        }
    }

    let reqs = stream::iter(reqs);

    // Will return in ordered fashion, so we know the address in the `called` list
    // corresponds to the task handle at the same index.
    let mut responses = senders.call_all(reqs);

    // Run the tasks concurrently.
    let mut i = 0;
    while let Some(result) = responses.next().await {
        let addr = &sent_addresses[i];
        match result {
            Ok(id) => {
                // Reply to the originating message with the address
                let span = tracing::info_span!("send", address = %addr);
                span.in_scope(|| {
                    tracing::info!(id = %id, "send request succeeded");
                });
                succeeded.push((*addr.clone(), id));
            }
            // By default, anyhow::Error's Display impl only prints the outermost error;
            // using the alternate formate specifier prints the entire chain of causes.
            Err(e) => {
                tracing::error!(?addr, ?e, "Failed to send funds");
                failed.push((*addr.clone(), format!("{:#}", e)))
            }
        }

        i += 1;
    }

    // Separate the rest of the list into unparsed and remaining valid ones
    let mut remaining = Vec::<Address>::new();
    for addr in addresses {
        match addr {
            AddressOrAlmost::Address(addr) => remaining.push(*addr),
            AddressOrAlmost::Almost(addr) => unparsed.push(addr),
        }
    }

    Ok(Response {
        succeeded,
        failed,
        unparsed,
        remaining,
    })
}
