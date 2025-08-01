//! Control interface client

use core::fmt::{self, Debug};
use core::time::Duration;

use std::collections::{BTreeMap, HashMap};

use async_nats::Subscriber;
use cloudevents::event::Event;
use futures::{StreamExt, TryFutureExt};
use serde::de::DeserializeOwned;
use tokio::sync::mpsc::Receiver;
use tracing::{debug, error, instrument, trace};

use crate::types::ctl::{
    CtlResponse, ScaleComponentCommand, StartProviderCommand, StopHostCommand, StopProviderCommand,
    UpdateComponentCommand,
};
use crate::types::host::{Host, HostInventory, HostLabel};
use crate::types::link::Link;
use crate::types::registry::RegistryCredential;
use crate::types::rpc::{
    ComponentAuctionAck, ComponentAuctionRequest, DeleteInterfaceLinkDefinitionRequest,
    ProviderAuctionAck, ProviderAuctionRequest,
};
use crate::{
    broker, json_deserialize, json_serialize, otel, HostLabelIdentifier, IdentifierKind, Result,
};

/// A client builder that can be used to fluently provide configuration settings used to construct
/// the control interface client
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClientBuilder {
    nc: async_nats::Client,
    topic_prefix: Option<String>,
    lattice: String,
    timeout: Duration,
    auction_timeout: Duration,
}

impl ClientBuilder {
    /// Creates a new client builder using the given client with all configuration values set to
    /// their defaults
    #[must_use]
    pub fn new(nc: async_nats::Client) -> ClientBuilder {
        ClientBuilder {
            nc,
            topic_prefix: None,
            lattice: "default".to_string(),
            timeout: Duration::from_secs(2),
            auction_timeout: Duration::from_secs(5),
        }
    }

    /// Sets the topic prefix for the NATS topic used for all control requests. Not to be confused
    /// with lattice ID/prefix
    #[must_use]
    pub fn topic_prefix(self, prefix: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            topic_prefix: Some(prefix.into()),
            ..self
        }
    }

    /// The lattice ID/prefix used for this client. If this function is not invoked, the prefix will
    /// be set to `default`
    #[must_use]
    pub fn lattice(self, prefix: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            lattice: prefix.into(),
            ..self
        }
    }

    /// Sets the timeout for control interface requests issued by the client. If not set, the
    /// default will be 2 seconds
    #[must_use]
    pub fn timeout(self, timeout: Duration) -> ClientBuilder {
        ClientBuilder { timeout, ..self }
    }

    /// Sets the timeout for auction (scatter/gather) operations. If not set, the default will be 5
    /// seconds
    #[must_use]
    pub fn auction_timeout(self, timeout: Duration) -> ClientBuilder {
        ClientBuilder {
            auction_timeout: timeout,
            ..self
        }
    }

    /// Constructs the client with the given configuration from the builder
    #[must_use]
    pub fn build(self) -> Client {
        Client {
            nc: self.nc,
            topic_prefix: self.topic_prefix,
            lattice: self.lattice,
            timeout: self.timeout,
            auction_timeout: self.auction_timeout,
        }
    }
}

/// Lattice control interface client
#[derive(Clone)]
#[non_exhaustive]
pub struct Client {
    /// Internal `async-nats` client
    nc: async_nats::Client,
    /// Topic prefix that should be used with this lattice control client
    topic_prefix: Option<String>,
    /// Lattice prefix
    lattice: String,
    /// Timeout
    timeout: Duration,
    /// Timeout to use when limiting auctions
    auction_timeout: Duration,
}

impl Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("topic_prefix", &self.topic_prefix)
            .field("lattice", &self.lattice)
            .field("timeout", &self.timeout)
            .field("auction_timeout", &self.auction_timeout)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Convenience method for creating a new client with all default settings. This is the same as
    /// calling `ClientBuilder::new(nc).build()`
    #[must_use]
    pub fn new(nc: async_nats::Client) -> Client {
        ClientBuilder::new(nc).build()
    }

    /// Get a copy of the NATS client in use by this control client
    #[allow(unused)]
    #[must_use]
    pub fn nats_client(&self) -> async_nats::Client {
        self.nc.clone()
    }

    /// Retrieve the lattice in use by the [`Client`]
    pub fn lattice(&self) -> &str {
        self.lattice.as_ref()
    }

    /// Perform a request with a timeout
    #[instrument(level = "debug", skip_all)]
    pub(crate) async fn request_timeout(
        &self,
        subject: String,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<async_nats::Message> {
        match tokio::time::timeout(
            timeout,
            self.nc.request_with_headers(
                subject,
                otel::HeaderInjector::default_with_span().into(),
                payload.into(),
            ),
        )
        .await
        {
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out").into()),
            Ok(Ok(message)) => Ok(message),
            Ok(Err(e)) => Err(e.into()),
        }
    }

    /// Queries the lattice for all responsive hosts, waiting for the full period specified by
    /// _timeout_.
    #[instrument(level = "debug", skip_all)]
    pub async fn get_hosts(&self) -> Result<Vec<CtlResponse<Host>>> {
        let subject = broker::v1::queries::hosts(&self.topic_prefix, &self.lattice);
        debug!("get_hosts:publish {}", &subject);
        self.publish_and_wait(subject, Vec::new()).await
    }

    /// Retrieves the contents of a running host
    #[instrument(level = "debug", skip_all)]
    pub async fn get_host_inventory(&self, host_id: &str) -> Result<CtlResponse<HostInventory>> {
        let subject = broker::v1::queries::host_inventory(
            &self.topic_prefix,
            &self.lattice,
            IdentifierKind::is_host_id(host_id)?.as_str(),
        );
        debug!("get_host_inventory:request {}", &subject);
        match self.request_timeout(subject, vec![], self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive host inventory from target host: {e}").into()),
        }
    }

    /// Retrieves the full set of all cached claims in the lattice.
    #[instrument(level = "debug", skip_all)]
    pub async fn get_claims(&self) -> Result<CtlResponse<Vec<HashMap<String, String>>>> {
        let subject = broker::v1::queries::claims(&self.topic_prefix, &self.lattice);
        debug!("get_claims:request {}", &subject);
        match self.request_timeout(subject, vec![], self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive claims from lattice: {e}").into()),
        }
    }

    /// Performs an component auction within the lattice, publishing a set of constraints and the
    /// metadata for the component in question. This will always wait for the full period specified by
    /// _duration_, and then return the set of gathered results. It is then up to the client to
    /// choose from among the "auction winners" to issue the appropriate command to start an component.
    /// Clients cannot assume that auctions will always return at least one result.
    #[instrument(level = "debug", skip_all)]
    pub async fn perform_component_auction(
        &self,
        component_ref: &str,
        component_id: &str,
        constraints: impl Into<BTreeMap<String, String>>,
    ) -> Result<Vec<CtlResponse<ComponentAuctionAck>>> {
        let subject = broker::v1::component_auction_subject(&self.topic_prefix, &self.lattice);
        let bytes = json_serialize(
            ComponentAuctionRequest::builder()
                .component_ref(IdentifierKind::is_component_ref(component_ref)?)
                .component_id(IdentifierKind::is_component_id(component_id)?)
                .constraints(constraints.into())
                .build()?,
        )?;
        debug!("component_auction:publish {}", &subject);
        self.publish_and_wait(subject, bytes).await
    }

    /// Performs a provider auction within the lattice, publishing a set of constraints and the
    /// metadata for the provider in question.
    ///
    /// This will always wait for the full period specified by _duration_, and then return the set of gathered results.
    /// It is then up to the client to choose from among the "auction winners" and issue the appropriate command to start a
    /// provider.
    ///
    /// Clients should not assume that auctions will always return at least one result.
    ///
    /// # Arguments
    ///
    /// * `provider_ref` - The ID of the provider to auction
    /// * `provider_id` - The ID of the provider auction
    /// * `constraints` - Constraints that govern where the provider can be placed
    ///
    #[instrument(level = "debug", skip_all)]
    pub async fn perform_provider_auction(
        &self,
        provider_ref: &str,
        provider_id: &str,
        constraints: impl Into<BTreeMap<String, String>>,
    ) -> Result<Vec<CtlResponse<ProviderAuctionAck>>> {
        let subject = broker::v1::provider_auction_subject(&self.topic_prefix, &self.lattice);
        let bytes = json_serialize(
            ProviderAuctionRequest::builder()
                .provider_ref(IdentifierKind::is_provider_ref(provider_ref)?)
                .provider_id(IdentifierKind::is_provider_id(provider_id)?)
                .constraints(constraints.into())
                .build()?,
        )?;
        debug!("provider_auction:publish {}", &subject);
        self.publish_and_wait(subject, bytes).await
    }

    /// Sends a request to the given host to scale a given component.
    ///
    /// This returns an acknowledgement of _receipt_ of the command, not a confirmation that the component scaled.
    /// An acknowledgement will either indicate some form of validation failure, or, if no failure occurs, the receipt of
    /// the command.
    ///
    /// To avoid blocking consumers, wasmCloud hosts will acknowledge the scale component
    /// command prior to fetching the component's OCI bytes.
    ///
    /// Client that need deterministic results as to whether the component completed its startup process
    /// must monitor the appropriate event in the control event stream.
    ///
    /// # Arguments
    ///
    /// * `host_id` - The ID of the host to scale the component on
    /// * `component_ref` - The OCI reference of the component to scale
    /// * `max_instances` - The maximum number of instances this component can run concurrently. Specifying `0` will stop the component.
    /// * `annotations` - Optional annotations to apply to the component
    /// * `config` - List of named configuration to use for the component
    /// * `allow_update` - Whether to perform allow updates to the component (triggering a separate update)
    ///
    #[instrument(level = "debug", skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub async fn scale_component(
        &self,
        host_id: &str,
        component_ref: &str,
        component_id: &str,
        max_instances: u32,
        annotations: Option<BTreeMap<String, String>>,
        config: Vec<String>,
    ) -> Result<CtlResponse<()>> {
        let host_id = IdentifierKind::is_host_id(host_id)?;
        let subject = broker::v1::commands::scale_component(
            &self.topic_prefix,
            &self.lattice,
            host_id.as_str(),
        );
        debug!("scale_component:request {}", &subject);
        let bytes = json_serialize(ScaleComponentCommand {
            max_instances,
            component_ref: IdentifierKind::is_component_ref(component_ref)?,
            component_id: IdentifierKind::is_component_id(component_id)?,
            host_id,
            annotations,
            config,
            ..Default::default()
        })?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive scale component acknowledgement: {e}").into()),
        }
    }

    /// Publishes a registry credential map to the control interface of the lattice.
    ///
    /// All hosts will be listening and overwrite their registry credential maps with the new information.
    ///
    /// It is highly recommended you use TLS connections with NATS and isolate the control interface
    /// credentials when using this function in production as the data contains secrets
    ///
    /// # Arguments
    ///
    /// * `registries` - A map of registry names to their credentials to be used for fetching from specific registries
    ///
    #[instrument(level = "debug", skip_all)]
    pub async fn put_registries(
        &self,
        registries: HashMap<String, RegistryCredential>,
    ) -> Result<CtlResponse<()>> {
        let subject = broker::v1::publish_registries(&self.topic_prefix, &self.lattice);
        debug!("put_registries:publish {}", &subject);
        let bytes = json_serialize(&registries)?;
        let resp = self
            .nc
            .publish_with_headers(
                subject,
                otel::HeaderInjector::default_with_span().into(),
                bytes.into(),
            )
            .await;
        if let Err(e) = resp {
            Err(format!("Failed to push registry credential map: {e}").into())
        } else {
            Ok(CtlResponse::<()>::success(
                "successfully added registries".into(),
            ))
        }
    }

    /// Puts a link into the lattice.
    ///
    /// # Errors
    ///
    /// Returns an error if it was unable to put the link
    #[instrument(level = "debug", skip_all)]
    pub async fn put_link(&self, link: Link) -> Result<CtlResponse<()>> {
        // Validate link parameters
        IdentifierKind::is_component_id(&link.source_id)?;
        IdentifierKind::is_component_id(&link.target)?;
        IdentifierKind::is_link_name(&link.name)?;

        let subject = broker::v1::put_link(&self.topic_prefix, &self.lattice);
        debug!("put_link:request {}", &subject);

        let bytes = crate::json_serialize(link)?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive put link acknowledgement: {e}").into()),
        }
    }

    /// Deletes a link from the lattice metadata keyvalue bucket.
    ///
    /// This is an idempotent operation.
    ///
    /// # Errors
    ///
    /// Returns an error if it was unable to delete.
    #[instrument(level = "debug", skip_all)]
    pub async fn delete_link(
        &self,
        source_id: &str,
        link_name: &str,
        wit_namespace: &str,
        wit_package: &str,
    ) -> Result<CtlResponse<()>> {
        let subject = broker::v1::delete_link(&self.topic_prefix, &self.lattice);
        let ld = DeleteInterfaceLinkDefinitionRequest::from_source_and_link_metadata(
            &IdentifierKind::is_component_id(source_id)?,
            &IdentifierKind::is_link_name(link_name)?,
            wit_namespace,
            wit_package,
        );
        let bytes = crate::json_serialize(&ld)?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive delete link acknowledgement: {e}").into()),
        }
    }

    /// Retrieves the list of link definitions stored in the lattice metadata key-value bucket.
    ///
    /// If the client was created with caching, this will return the cached list of links. Otherwise,
    /// it will query the bucket for the list of links.
    ///
    #[instrument(level = "debug", skip_all)]
    pub async fn get_links(&self) -> Result<CtlResponse<Vec<Link>>> {
        let subject = broker::v1::queries::link_definitions(&self.topic_prefix, &self.lattice);
        debug!("get_links:request {}", &subject);
        match self.request_timeout(subject, vec![], self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive a response to get links: {e}").into()),
        }
    }

    /// Puts a named config, replacing any data that is already present.
    ///
    /// Config names must be valid NATS subject strings and not contain any `.` or `>` characters.
    ///
    /// # Arguments
    ///
    /// * `config_name` - Name of the configuration that should be saved
    /// * `config` - contents of the configuration to be saved
    ///
    #[instrument(level = "debug", skip_all)]
    pub async fn put_config(
        &self,
        config_name: &str,
        config: impl Into<HashMap<String, String>>,
    ) -> Result<CtlResponse<()>> {
        let subject = broker::v1::put_config(&self.topic_prefix, &self.lattice, config_name);
        debug!(%subject, %config_name, "Putting config");
        let data = serde_json::to_vec(&config.into())?;
        match self.request_timeout(subject, data, self.timeout).await {
            Ok(msg) => json_deserialize(&msg.payload),
            Err(e) => Err(format!("Did not receive a response to put config request: {e}").into()),
        }
    }

    /// Delete the named config item.
    ///
    /// Config names must be valid NATS subject strings and not contain any `.` or `>` characters.
    ///
    /// # Arguments
    ///
    /// * `config_name` - Name of the configuration that should be deleted
    ///
    #[instrument(level = "debug", skip_all)]
    pub async fn delete_config(&self, config_name: &str) -> Result<CtlResponse<()>> {
        let subject = broker::v1::delete_config(&self.topic_prefix, &self.lattice, config_name);
        debug!(%subject, %config_name, "Delete config");
        match self
            .request_timeout(subject, Vec::default(), self.timeout)
            .await
        {
            Ok(msg) => json_deserialize(&msg.payload),
            Err(e) => {
                Err(format!("Did not receive a response to delete config request: {e}").into())
            }
        }
    }

    /// Get the named config item.
    ///
    /// # Arguments
    ///
    /// * `config_name` -  The name of the config to fetch. Config names must be valid NATS subject strings and not contain any `.` or `>` characters.
    ///
    /// # Returns
    ///
    /// A map of key-value pairs representing the contents of the config item. This response is wrapped in the [CtlResponse] type. If
    /// the config item does not exist, the host will return a [CtlResponse] with a `success` field set to `true` and a `response` field
    /// set to [Option::None]. If the config item exists, the host will return a [CtlResponse] with a `success` field set to `true` and a
    /// `response` field set to [Option::Some] containing the key-value pairs of the config item.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::collections::HashMap;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let nc_client = async_nats::connect("127.0.0.1:4222").await.expect("failed to build NATS client");
    /// let ctl_client = wasmcloud_control_interface::Client::new(nc_client);
    /// ctl_client.put_config(
    ///     "foo",
    ///     HashMap::from_iter(vec![("key".to_string(), "value".to_string())]),
    /// )
    /// .await
    /// .expect("should be able to put config");
    ///
    /// let config_resp = ctl_client.get_config("foo").await.expect("should be able to get config");
    /// assert!(config_resp.succeeded());
    /// assert_eq!(config_resp.data(), Some(&HashMap::from_iter(vec![("key".to_string(), "value".to_string())])));
    ///
    /// // Note that the host will return a success response even if the config item does not exist.
    /// // Errors are reserved for communication problems with the host or with the config store.
    /// let absent_config_resp = ctl_client.get_config("bar").await.expect("should be able to get config");
    /// assert!(absent_config_resp.succeeded());
    /// assert_eq!(absent_config_resp.data(), None);
    ///
    /// # }
    /// ```
    ///
    #[instrument(level = "debug", skip_all)]
    pub async fn get_config(
        &self,
        config_name: &str,
    ) -> Result<CtlResponse<HashMap<String, String>>> {
        let subject = broker::v1::queries::config(&self.topic_prefix, &self.lattice, config_name);
        debug!(%subject, %config_name, "Getting config");
        match self
            .request_timeout(subject, Vec::default(), self.timeout)
            .await
        {
            Ok(msg) => json_deserialize(&msg.payload),
            Err(e) => Err(format!("Did not receive a response to get config request: {e}").into()),
        }
    }

    /// Put a new (or update an existing) label on the given host.
    ///
    /// # Arguments
    ///
    /// * `host_id` - ID of the host on which the label should be placed
    /// * `key` - The key of the label
    /// * `value` - The value of the label
    ///
    /// # Errors
    ///
    /// Will return an error if there is a communication problem with the host
    ///
    pub async fn put_label(
        &self,
        host_id: &str,
        key: &str,
        value: &str,
    ) -> Result<CtlResponse<()>> {
        let subject = broker::v1::put_label(&self.topic_prefix, &self.lattice, host_id);
        debug!(%subject, "putting label");
        let bytes = json_serialize(HostLabel {
            key: key.to_string(),
            value: value.to_string(),
        })?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive put label acknowledgement: {e}").into()),
        }
    }

    /// Removes a label from the given host.
    ///
    /// # Arguments
    ///
    /// * `host_id` - ID of the host on which the label should be deleted
    /// * `key` - The key of the label that should be deleted
    ///
    /// # Errors
    ///
    /// Will return an error if there is a communication problem with the host
    ///
    pub async fn delete_label(&self, host_id: &str, key: &str) -> Result<CtlResponse<()>> {
        let subject = broker::v1::delete_label(&self.topic_prefix, &self.lattice, host_id);
        debug!(%subject, "removing label");
        let bytes = json_serialize(HostLabelIdentifier {
            key: key.to_string(),
        })?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive remove label acknowledgement: {e}").into()),
        }
    }

    /// Command a host to replace an existing component with a new component indicated by an OCI image reference.
    ///
    /// The host will acknowledge this request as soon as it verifies that the target component is running.
    ///
    /// Note that acknowledgement occurs **before** the new bytes are downloaded. Live-updating an component can take a long time
    /// and control clients cannot block waiting for a reply that could come several seconds later.
    ///
    /// To properly verify that a component has been updated, create  listener for the appropriate [`PublishedEvent`] on the
    /// control events channel
    ///
    /// # Arguments
    ///
    /// * `host_id` - ID of the host on which the component should be updated
    /// * `existing_component_id` - ID of the existing component
    /// * `new_component_ref` - New component reference that should be used
    /// * `annotations` - Annotations to place on the newly updated component
    ///
    #[instrument(level = "debug", skip_all)]
    pub async fn update_component(
        &self,
        host_id: &str,
        existing_component_id: &str,
        new_component_ref: &str,
        annotations: Option<BTreeMap<String, String>>,
    ) -> Result<CtlResponse<()>> {
        let host_id = IdentifierKind::is_host_id(host_id)?;
        let subject = broker::v1::commands::update_component(
            &self.topic_prefix,
            &self.lattice,
            host_id.as_str(),
        );
        debug!("update_component:request {}", &subject);
        let bytes = json_serialize(UpdateComponentCommand {
            host_id,
            component_id: IdentifierKind::is_component_id(existing_component_id)?,
            new_component_ref: IdentifierKind::is_component_ref(new_component_ref)?,
            annotations,
        })?;
        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive update component acknowledgement: {e}").into()),
        }
    }

    /// Command a host to start a provider with a given OCI reference.
    ///
    /// The specified link name will be used (or "default" if none is specified).
    ///
    /// The target wasmCloud host will acknowledge the receipt of this command _before_ downloading the provider's bytes from the
    /// OCI registry, indicating either a validation failure or success.
    ///
    /// Clients that need deterministic guarantees that the provider has completed its startup process, should
    /// monitor the control event stream for the appropriate event.
    ///
    /// The `provider_configuration` parameter is a list of named configs to use for this provider, and configurations are not required.
    ///
    /// # Arguments
    ///
    /// * `host_id` - ID of the host on which to start the provider
    /// * `provider_ref` - Image reference of the provider to start
    /// * `provider_id` - ID of the provider to start
    /// * `annotations` - Annotations to place on the started provider
    /// * `provider_configuration` - Configuration relevant to the provider (if any)
    ///
    #[instrument(level = "debug", skip_all)]
    pub async fn start_provider(
        &self,
        host_id: &str,
        provider_ref: &str,
        provider_id: &str,
        annotations: Option<BTreeMap<String, String>>,
        provider_configuration: Vec<String>,
    ) -> Result<CtlResponse<()>> {
        let host_id = IdentifierKind::is_host_id(host_id)?;
        let subject = broker::v1::commands::start_provider(
            &self.topic_prefix,
            &self.lattice,
            host_id.as_str(),
        );
        debug!("start_provider:request {}", &subject);
        let mut cmd = StartProviderCommand::builder()
            .host_id(&host_id)
            .provider_ref(&IdentifierKind::is_provider_ref(provider_ref)?)
            .provider_id(&IdentifierKind::is_component_id(provider_id)?);
        if let Some(annotations) = annotations {
            cmd = cmd.annotations(annotations);
        }
        let cmd = cmd.config(provider_configuration).build()?;
        let bytes = json_serialize(cmd)?;

        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive start provider acknowledgement: {e}").into()),
        }
    }

    /// Issues a command to a host to stop a provider for the given OCI reference, link name, and
    /// contract ID.
    ///
    /// The target wasmCloud host will acknowledge the receipt of this command, and
    /// _will not_ supply a discrete confirmation that a provider has terminated. For that kind of
    /// information, the client must also monitor the control event stream
    ///
    /// # Arguments
    ///
    /// * `host_id` - ID of the host on which to stop the provider
    /// * `provider_id` - ID of the provider to stop
    ///
    #[instrument(level = "debug", skip_all)]
    pub async fn stop_provider(&self, host_id: &str, provider_id: &str) -> Result<CtlResponse<()>> {
        let host_id = IdentifierKind::is_host_id(host_id)?;

        let subject = broker::v1::commands::stop_provider(
            &self.topic_prefix,
            &self.lattice,
            host_id.as_str(),
        );
        debug!("stop_provider:request {}", &subject);
        let bytes = json_serialize(StopProviderCommand {
            host_id,
            provider_id: IdentifierKind::is_component_id(provider_id)?,
        })?;

        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive stop provider acknowledgement: {e}").into()),
        }
    }

    /// Issues a command to a specific host to perform a graceful termination.
    ///
    /// The target host will acknowledge receipt of the command before it attempts a shutdown.
    ///
    /// To deterministically verify that the host is down, a client should monitor for the "host stopped" event or
    /// passively detect the host down by way of a lack of heartbeat receipts
    ///
    /// # Arguments
    ///
    /// * `host_id` - ID of the host to stop
    /// * `timeout_ms` - (optional) amount of time to allow the host to complete stopping
    ///
    #[instrument(level = "debug", skip_all)]
    pub async fn stop_host(
        &self,
        host_id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<CtlResponse<()>> {
        let host_id = IdentifierKind::is_host_id(host_id)?;
        let subject =
            broker::v1::commands::stop_host(&self.topic_prefix, &self.lattice, host_id.as_str());
        debug!("stop_host:request {}", &subject);
        let bytes = json_serialize(StopHostCommand {
            host_id,
            timeout: timeout_ms,
        })?;

        match self.request_timeout(subject, bytes, self.timeout).await {
            Ok(msg) => Ok(json_deserialize(&msg.payload)?),
            Err(e) => Err(format!("Did not receive stop host acknowledgement: {e}").into()),
        }
    }

    /// Publish a message and wait for a response
    async fn publish_and_wait<D: DeserializeOwned>(
        &self,
        subject: String,
        payload: Vec<u8>,
    ) -> Result<Vec<D>> {
        let reply = self.nc.new_inbox();
        let sub = self.nc.subscribe(reply.clone()).await?;
        self.nc
            .publish_with_reply_and_headers(
                subject.clone(),
                reply,
                otel::HeaderInjector::default_with_span().into(),
                payload.into(),
            )
            .await?;
        let nc = self.nc.clone();
        tokio::spawn(async move {
            if let Err(error) = nc.flush().await {
                error!(%error, "flush after publish");
            }
        });
        Ok(collect_sub_timeout::<D>(sub, self.auction_timeout, subject.as_str()).await)
    }

    /// Returns the receiver end of a channel that subscribes to the lattice event stream.
    ///
    /// Any [`Event`]s that are published after this channel is created
    /// will be added to the receiver channel's buffer, which can be observed or handled if needed.
    ///
    /// See the example for how you could use this receiver to handle events.
    ///
    /// # Example
    ///
    /// ```rust
    /// use wasmcloud_control_interface::{Client, ClientBuilder};
    /// async {
    ///   let nc = async_nats::connect("127.0.0.1:4222").await.unwrap();
    ///   let client = ClientBuilder::new(nc)
    ///                 .timeout(std::time::Duration::from_millis(1000))
    ///                 .auction_timeout(std::time::Duration::from_millis(1000))
    ///                 .build();
    ///   let mut receiver = client.events_receiver(vec!["component_scaled".to_string()]).await.unwrap();
    ///   while let Some(evt) = receiver.recv().await {
    ///       println!("Event received: {:?}", evt);
    ///   }
    /// };
    /// ```
    ///
    /// # Arguments
    ///
    /// * `event_types` - List of types of events to listen for
    ///
    #[allow(clippy::missing_errors_doc)] // TODO: Document errors
    pub async fn events_receiver(&self, event_types: Vec<String>) -> Result<Receiver<Event>> {
        let (sender, receiver) = tokio::sync::mpsc::channel(5000);
        let futs = event_types.into_iter().map(|event_type| {
            self.nc
                .subscribe(format!("wasmbus.evt.{}.{}", self.lattice, event_type))
                .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)
        });
        let subs: Vec<Subscriber> = futures::future::join_all(futs)
            .await
            .into_iter()
            .collect::<Result<_>>()?;
        let mut stream = futures::stream::select_all(subs);
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let Ok(evt) = json_deserialize::<Event>(&msg.payload) else {
                    error!("Object received on event stream was not a CloudEvent");
                    continue;
                };
                trace!("received event: {:?}", evt);
                let Ok(()) = sender.send(evt).await else {
                    break;
                };
            }
        });
        Ok(receiver)
    }
}

/// Collect `T` values until timeout has elapsed
pub(crate) async fn collect_sub_timeout<T: DeserializeOwned>(
    mut sub: async_nats::Subscriber,
    timeout: Duration,
    reason: &str,
) -> Vec<T> {
    let mut items = Vec::new();
    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            msg = sub.next() => {
                let Some(msg) = msg else {
                    break;
                };
                if msg.payload.is_empty() {
                    break;
                }
                match json_deserialize::<T>(&msg.payload) {
                    Ok(item) => items.push(item),
                    Err(error) => {
                        error!(%reason, %error,
                            "deserialization error in auction - results may be incomplete",
                        );
                        break;
                    }
                }
            },
            () = &mut sleep => { /* timeout */ break; }
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Note: This test is a means of manually watching the event stream as CloudEvents are received
    /// It does not assert functionality, and so we've marked it as ignore to ensure it's not run by default
    /// It currently listens for 120 seconds then exits
    #[tokio::test]
    #[ignore]
    async fn test_events_receiver() {
        let nc = async_nats::connect("127.0.0.1:4222").await.unwrap();
        let client = ClientBuilder::new(nc)
            .timeout(Duration::from_millis(1000))
            .auction_timeout(Duration::from_millis(1000))
            .build();
        let mut receiver = client
            .events_receiver(vec!["foobar".to_string()])
            .await
            .unwrap();
        tokio::spawn(async move {
            while let Some(evt) = receiver.recv().await {
                println!("Event received: {evt:?}");
            }
        });
        println!("Listening to Cloud Events for 120 seconds. Then we will quit.");
        tokio::time::sleep(Duration::from_secs(120)).await;
    }

    #[test]
    fn test_check_identifier() -> Result<()> {
        assert!(IdentifierKind::is_host_id("").is_err());
        assert!(IdentifierKind::is_host_id(" ").is_err());
        let host_id = IdentifierKind::is_host_id("             ");
        assert!(host_id.is_err(), "parsing host id should have failed");
        assert!(host_id
            .unwrap_err()
            .to_string()
            .contains("Host ID cannot be empty"));
        let provider_ref = IdentifierKind::is_provider_ref("");
        assert!(
            provider_ref.is_err(),
            "parsing provider ref should have failed"
        );
        assert!(provider_ref
            .unwrap_err()
            .to_string()
            .contains("Provider OCI reference cannot be empty"));
        assert!(IdentifierKind::is_host_id("host_id").is_ok());
        let component_id = IdentifierKind::is_component_id("            iambatman  ")?;
        assert_eq!(component_id, "iambatman");

        Ok(())
    }

    #[tokio::test]
    #[ignore]
    /// Test after large 1.0 refcomponents to ensure all return types are formatted as [CtlResponse] types, and that
    /// the host can handle all of the requests.
    ///
    /// You must run NATS and one host locally to run this test successfully.
    async fn ctl_response_comprehensive() {
        let client = Client::new(
            async_nats::connect("127.0.0.1:4222")
                .await
                .expect("should be able to connect to local NATS"),
        );
        // Fetch the one host we ran
        let hosts = client
            .get_hosts()
            .await
            .expect("should be able to fetch at least a host");
        assert_eq!(hosts.len(), 1);
        let host = hosts.first().expect("one host to exist");
        assert!(host.success);
        assert!(host.message.is_empty());
        assert!(host.response.is_some());
        let host = host.response.as_ref().unwrap();
        ////
        // Actor operations
        ////
        // Actor Auction
        let auction_response = client
            .perform_component_auction(
                "ghcr.io/brooksmtownsend/http-hello-world-rust:0.1.0",
                "echo",
                BTreeMap::new(),
            )
            .await
            .expect("should be able to auction an component");
        assert_eq!(auction_response.len(), 1);
        let first_ack = auction_response.first().expect("a single component ack");
        let auction_ack = first_ack.response.as_ref().unwrap();
        let (component_ref, component_id) = (&auction_ack.component_ref, &auction_ack.component_id);
        // Actor Scale
        let scale_response = client
            .scale_component(
                &host.id,
                component_ref,
                component_id,
                1,
                None,
                Vec::with_capacity(0),
            )
            .await
            .expect("should be able to scale component");
        assert!(scale_response.success);
        assert!(scale_response.message.is_empty());
        assert!(scale_response.response.is_none());
        // Actor Update (TODO(brooksmtownsend): we should test this with a real update, but I'm using a failure case)
        let update_component_resp = client
            .update_component(
                &host.id,
                "nonexistantcomponentID",
                "ghcr.io/wasmcloud/components/http-keyvalue-counter-rust:0.1.0",
                None,
            )
            .await
            .expect("should be able to issue update component request");
        assert!(!update_component_resp.success);
        assert_eq!(
            update_component_resp.message,
            "component not found".to_string()
        );
        assert_eq!(update_component_resp.response, None);

        ////
        // Provider operations
        ////
        // Provider Auction
        let provider_acks = client
            .perform_provider_auction(
                "ghcr.io/wasmcloud/http-server:0.26.0",
                "httpserver",
                BTreeMap::new(),
            )
            .await
            .expect("should be able to hold provider auction");
        assert_eq!(provider_acks.len(), 1);
        let provider_ack = provider_acks.first().expect("a single provider ack");
        assert!(provider_ack.success);
        assert!(provider_ack.message.is_empty());
        assert!(provider_ack.response.is_some());
        let auction_ack = provider_ack.response.as_ref().unwrap();
        let (provider_ref, provider_id) = (&auction_ack.provider_ref, &auction_ack.provider_id);
        // Provider Start
        let start_response = client
            .start_provider(&host.id, provider_ref, provider_id, None, vec![])
            .await
            .expect("should be able to start provider");
        assert!(start_response.success);
        assert!(start_response.message.is_empty());
        assert!(start_response.response.is_none());
        // Provider Stop (TODO(brooksmtownsend): not enough time to let the provider really stop, so I'm using a failure case)
        let stop_response = client
            .stop_provider(&host.id, "notarealproviderID")
            .await
            .expect("should be able to issue stop provider request");
        assert!(!stop_response.success);
        assert_eq!(
            stop_response.message,
            "provider with that ID is not running".to_string()
        );
        assert!(stop_response.response.is_none());
        ////
        // Link operations
        ////
        tokio::time::sleep(Duration::from_secs(5)).await;
        // Link Put
        let link_put = client
            .put_link(Link {
                source_id: "echo".to_string(),
                target: "httpserver".to_string(),
                name: "default".to_string(),
                wit_namespace: "wasi".to_string(),
                wit_package: "http".to_string(),
                interfaces: vec!["incoming-handler".to_string()],
                ..Default::default()
            })
            .await
            .expect("should be able to put link");
        assert!(link_put.success);
        assert!(link_put.message.is_empty());
        assert!(link_put.response.is_none());
        let links_get = client
            .get_links()
            .await
            .expect("should be able to get links");
        assert!(links_get.success);
        assert!(links_get.message.is_empty());
        assert!(links_get.response.is_some());
        // Link Get
        let link_get = links_get.response.as_ref().unwrap().first().unwrap();
        assert_eq!(link_get.source_id, "echo");
        assert_eq!(link_get.target, "httpserver");
        assert_eq!(link_get.name, "default");
        assert_eq!(link_get.wit_namespace, "wasi");
        assert_eq!(link_get.wit_package, "http");
        // Link Del
        let link_del = client
            .delete_link("echo", "default", "wasi", "http")
            .await
            .expect("should be able to delete link");
        assert!(link_del.success);
        assert!(link_del.message.is_empty());
        assert!(link_del.response.is_none());

        ////
        // Label operations
        ////
        // Label Put
        let label_one = client
            .put_label(&host.id, "idk", "lol")
            .await
            .expect("should be able to put label");
        assert!(label_one.success);
        assert!(label_one.message.is_empty());
        assert!(label_one.response.is_none());
        let label_two = client
            .put_label(&host.id, "foo", "bar")
            .await
            .expect("should be able to put another label");
        assert!(label_two.success);
        assert!(label_two.message.is_empty());
        assert!(label_two.response.is_none());
        // Label Del
        let del_label_one = client
            .delete_label(&host.id, "idk")
            .await
            .expect("should be able to delete label");
        assert!(del_label_one.success);
        assert!(del_label_one.message.is_empty());
        assert!(del_label_one.response.is_none());
        ////
        // Registry operations
        ////
        // Registry Put
        let registry_put = client
            .put_registries(HashMap::from_iter([(
                "mycloud.io".to_string(),
                RegistryCredential {
                    username: Some("user".to_string()),
                    password: Some("pass".to_string()),
                    registry_type: "oci".to_string(),
                    token: None,
                },
            )]))
            .await
            .expect("should be able to put registries");
        assert!(registry_put.success);
        assert!(registry_put.message.is_empty());
        assert!(registry_put.response.is_none());

        ////
        // Config operations
        ////
        // Config Put
        let config_put = client
            .put_config(
                "test_config",
                HashMap::from_iter([("sup".to_string(), "hey".to_string())]),
            )
            .await
            .expect("should be able to put config");
        assert!(config_put.success);
        assert!(config_put.message.is_empty());
        assert!(config_put.response.is_none());
        // Config Get
        let config_get = client
            .get_config("test_config")
            .await
            .expect("should be able to get config");
        assert!(config_get.success);
        assert!(config_get.message.is_empty());
        assert!(config_get
            .response
            .is_some_and(|r| r.get("sup").is_some_and(|s| s == "hey")));
        // Config Del
        let config_del = client
            .delete_config("test_config")
            .await
            .expect("should be able to delete config");
        assert!(config_del.success);
        assert!(config_del.message.is_empty());
        assert!(config_del.response.is_none());

        ////
        // Host operations
        ////
        // Host Get
        let inventory = client
            .get_host_inventory(&host.id)
            .await
            .expect("should be able to fetch at least a host");
        assert!(inventory.success);
        assert!(inventory.message.is_empty());
        assert!(inventory.response.is_some());
        let host_inventory = inventory.response.unwrap();
        assert!(host_inventory.components.iter().all(|a| a.id == "echo"));
        assert!(!host_inventory.labels.contains_key("idk"));
        assert!(host_inventory
            .labels
            .get("foo")
            .is_some_and(|f| f == &"bar".to_string()));
        // Host Stop
        let stop_host = client
            .stop_host(&host.id, Some(1234))
            .await
            .expect("should be able to stop host");
        assert!(stop_host.success);
        assert!(stop_host.message.is_empty());
        assert!(stop_host.response.is_none());
    }
}
