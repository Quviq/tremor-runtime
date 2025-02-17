// Copyright 2022, The Tremor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::meta;

use crate::connectors::google::{AuthInterceptor, ChannelFactory, TokenProvider};
use crate::connectors::impls::gcl::writer::Config;
use crate::connectors::prelude::*;
use crate::connectors::sink::concurrency_cap::ConcurrencyCap;
use crate::connectors::utils::pb;
use async_std::channel::Sender;
use async_std::prelude::FutureExt;
use googapis::google::logging::v2::log_entry::Payload;
use googapis::google::logging::v2::logging_service_v2_client::LoggingServiceV2Client;
use googapis::google::logging::v2::{LogEntry, WriteLogEntriesRequest};
use prost_types::Timestamp;
use std::time::Duration;
use tonic::codegen::InterceptedService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};
use tonic::Code;
use tremor_common::time::nanotime;

pub(crate) struct TonicChannelFactory;

#[async_trait::async_trait]
impl ChannelFactory<Channel> for TonicChannelFactory {
    async fn make_channel(&self, connect_timeout: Duration) -> Result<Channel> {
        let tls_config = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(googapis::CERTIFICATES))
            .domain_name("logging.googleapis.com");

        Ok(Channel::from_static("https://logging.googleapis.com")
            .connect_timeout(connect_timeout)
            .tls_config(tls_config)?
            .connect()
            .await?)
    }
}

pub(crate) struct GclSink<T, TChannel>
where
    T: TokenProvider + Clone,
    TChannel: tonic::codegen::Service<
            http::Request<tonic::body::BoxBody>,
            Response = http::Response<tonic::transport::Body>,
        > + Clone,
{
    client: Option<LoggingServiceV2Client<InterceptedService<TChannel, AuthInterceptor<T>>>>,
    config: Config,
    concurrency_cap: ConcurrencyCap,
    reply_tx: Sender<AsyncSinkReply>,
    channel_factory: Box<dyn ChannelFactory<TChannel> + Send + Sync>,
}

fn value_to_log_entry(
    timestamp: Timestamp,
    config: &Config,
    data: &Value,
    meta: Option<&Value>,
) -> Result<LogEntry> {
    Ok(LogEntry {
        log_name: config.log_name(meta),
        resource: super::value_to_monitored_resource(config.resource.as_ref())?,
        timestamp: Some(timestamp),
        receive_timestamp: None,
        severity: config.log_severity(meta)?,
        insert_id: meta::insert_id(meta),
        http_request: meta::http_request(meta),
        labels: Config::labels(meta),
        operation: meta::operation(meta),
        trace: meta::trace(meta),
        span_id: meta::span_id(meta),
        trace_sampled: meta::trace_sampled(meta)?,
        source_location: meta::source_location(meta),
        payload: Some(Payload::JsonPayload(pb::value_to_prost_struct(data)?)),
    })
}

impl<
        T: TokenProvider + Clone,
        TChannel: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<tonic::transport::Body>,
                Error = TChannelError,
            > + Send
            + Clone,
        TChannelError: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    > GclSink<T, TChannel>
{
    pub fn new(
        config: Config,
        reply_tx: Sender<AsyncSinkReply>,
        channel_factory: impl ChannelFactory<TChannel> + Send + Sync + 'static,
    ) -> Self {
        let concurrency_cap = ConcurrencyCap::new(config.concurrency, reply_tx.clone());
        Self {
            client: None,
            config,
            concurrency_cap,
            reply_tx,
            channel_factory: Box::new(channel_factory),
        }
    }
}

#[async_trait::async_trait]
impl<
        T: TokenProvider + Clone + 'static,
        TChannel: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<tonic::transport::Body>,
                Error = TChannelError,
            > + Send
            + Clone
            + 'static,
        TChannelError: Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send + Sync,
    > Sink for GclSink<T, TChannel>
where
    TChannel::Future: Send,
{
    async fn on_event(
        &mut self,
        _input: &str,
        event: Event,
        ctx: &SinkContext,
        _serializer: &mut EventSerializer,
        start: u64,
    ) -> Result<SinkReply> {
        let client = self.client.as_mut().ok_or(ErrorKind::ClientNotAvailable(
            "Google Cloud Logging",
            "The client is not connected",
        ))?;

        let mut entries = Vec::with_capacity(event.len());
        for (data, meta) in event.value_meta_iter() {
            let meta = meta.get("gcl_writer").or(None);
            #[allow(clippy::cast_precision_loss)]
            #[allow(clippy::cast_possible_wrap)]
            let mut timestamp = Timestamp {
                seconds: (event.ingest_ns / 1_000_000_000) as i64,
                nanos: (event.ingest_ns % 1_000_000_000) as i32,
            };
            timestamp.normalize();
            entries.push(value_to_log_entry(timestamp, &self.config, data, meta)?);
        }

        let reply_tx = self.reply_tx.clone();
        let guard = self.concurrency_cap.inc_for(&event).await?;
        let log_name = self.config.log_name(None);
        let resource = super::value_to_monitored_resource(self.config.resource.as_ref())?;
        let labels = self.config.labels.clone();
        let partial_success = self.config.partial_success;
        let dry_run = self.config.dry_run;
        let request_timeout = Duration::from_nanos(self.config.request_timeout);
        let task_ctx = ctx.clone();
        let mut task_client = client.clone();

        spawn_task(ctx.clone(), async move {
            let log_entries_response = task_client
                .write_log_entries(WriteLogEntriesRequest {
                    log_name,
                    resource,
                    labels,
                    entries,
                    partial_success,
                    dry_run,
                })
                .timeout(request_timeout)
                .await?;

            if let Err(error) = log_entries_response {
                error!("Failed to write a log entries: {}", error);

                if matches!(
                    error.code(),
                    Code::Aborted
                        | Code::Cancelled
                        | Code::DataLoss
                        | Code::DeadlineExceeded
                        | Code::Internal
                        | Code::ResourceExhausted
                        | Code::Unavailable
                        | Code::Unknown
                ) {
                    task_ctx.swallow_err(
                        task_ctx.notifier.connection_lost().await,
                        "Failed to notify about Google Cloud Logging connection loss",
                    );
                }

                reply_tx
                    .send(AsyncSinkReply::Fail(ContraflowData::from(event)))
                    .await?;
            } else {
                reply_tx
                    .send(AsyncSinkReply::Ack(
                        ContraflowData::from(event),
                        nanotime() - start,
                    ))
                    .await?;
            }

            drop(guard);
            Ok(())
        });

        Ok(SinkReply::NONE)
    }

    async fn connect(&mut self, ctx: &SinkContext, _attempt: &Attempt) -> Result<bool> {
        info!("{} Connecting to Google Cloud Logging", ctx);
        let channel = self
            .channel_factory
            .make_channel(Duration::from_nanos(self.config.connect_timeout))
            .await?;

        let client = LoggingServiceV2Client::with_interceptor(
            channel,
            AuthInterceptor {
                token_provider: T::default(),
            },
        );

        self.client = Some(client);

        Ok(true)
    }

    fn auto_ack(&self) -> bool {
        false
    }

    fn asynchronous(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[cfg(feature = "gcp-integration")]
mod test {
    #![allow(clippy::cast_possible_wrap)]
    use super::*;
    use crate::connectors::impls::gcl;
    use crate::connectors::tests::ConnectorHarness;
    use crate::connectors::ConnectionLostNotifier;
    use crate::connectors::{
        google::tests::TestTokenProvider, utils::quiescence::QuiescenceBeacon,
    };
    use async_std::channel::bounded;
    use bytes::Bytes;
    use futures::future::Ready;
    use googapis::google::logging::r#type::LogSeverity;
    use googapis::google::logging::v2::WriteLogEntriesResponse;
    use http::{HeaderMap, HeaderValue};
    use http_body::Body;
    use prost::Message;
    use std::task::Poll;
    use std::{
        collections::HashMap,
        fmt::{Debug, Display, Formatter},
    };
    use tonic::body::BoxBody;
    use tonic::codegen::Service;
    use tremor_common::ids::SinkId;
    use tremor_pipeline::CbAction::Trigger;
    use tremor_pipeline::EventId;
    use tremor_value::{literal, structurize};

    #[derive(Debug)]
    enum MockServiceError {}

    impl Display for MockServiceError {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            write!(f, "MockServiceError")
        }
    }

    impl std::error::Error for MockServiceError {}

    struct MockChannelFactory;

    #[async_trait::async_trait]
    impl ChannelFactory<MockService> for MockChannelFactory {
        async fn make_channel(&self, _connect_timeout: Duration) -> Result<MockService> {
            Ok(MockService {})
        }
    }

    #[derive(Clone)]
    struct MockService {}

    impl Service<http::Request<BoxBody>> for MockService {
        type Response = http::Response<tonic::transport::Body>;
        type Error = MockServiceError;
        type Future =
            Ready<std::result::Result<http::Response<tonic::transport::Body>, MockServiceError>>;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        #[allow(clippy::unwrap_used, clippy::cast_possible_truncation)] // We don't control the return type here
        fn call(&mut self, _request: http::Request<BoxBody>) -> Self::Future {
            let mut buffer = vec![];

            WriteLogEntriesResponse {}
                .encode_length_delimited(&mut buffer)
                .unwrap();
            let body = bytes::Bytes::from(buffer);
            let body = http_body::Full::new(body);
            let body = http_body::combinators::BoxBody::new(body).map_err(|err| match err {});
            let mut response = tonic::body::BoxBody::new(body);
            let (mut tx, body) = tonic::transport::Body::channel();
            let jh = async_std::task::spawn(async move {
                let response = response.data().await.unwrap().unwrap();
                let len: [u8; 4] = (response.len() as u32).to_ne_bytes();
                let len = Bytes::from(len.to_vec());
                tx.send_data(len).await.unwrap();
                tx.send_data(response).await.unwrap();
                let mut trailers = HeaderMap::new();
                trailers.insert(
                    "content-type",
                    HeaderValue::from_static("application/grpc+proto"),
                );
                trailers.insert("grpc-status", HeaderValue::from_static("0"));
                tx.send_trailers(trailers).await.unwrap();
            });
            async_std::task::spawn_blocking(|| jh);

            let response = http::Response::new(body);

            futures::future::ready(Ok(response))
        }
    }

    #[async_std::test]
    async fn on_event_can_send_an_event() -> Result<()> {
        let (tx, rx) = bounded(10);
        let (connection_lost_tx, _connection_lost_rx) = bounded(10);

        let mut sink = GclSink::<TestTokenProvider, _>::new(
            Config {
                log_name: None,
                resource: None,
                partial_success: false,
                dry_run: false,
                connect_timeout: 0,
                request_timeout: 0,
                default_severity: 0,
                labels: HashMap::default(),
                concurrency: 0,
            },
            tx,
            MockChannelFactory,
        );
        let sink_context = SinkContext {
            uid: SinkId::default(),
            alias: Alias::new("a", "b"),
            connector_type: ConnectorType::default(),
            quiescence_beacon: QuiescenceBeacon::default(),
            notifier: ConnectionLostNotifier::new(connection_lost_tx),
        };

        sink.connect(&sink_context, &Attempt::default()).await?;

        let event = Event {
            id: EventId::new(1, 2, 3, 4),
            data: EventPayload::from(ValueAndMeta::from(literal!({
                "message": "test",
                "severity": "INFO",
                "labels": {
                    "label1": "value1",
                    "label2": "value2"
                }
            }))),
            ..Default::default()
        };
        sink.on_event(
            "",
            event.clone(),
            &sink_context,
            &mut EventSerializer::new(
                None,
                CodecReq::Structured,
                vec![],
                &"a".into(),
                &Alias::new("a", "b"),
            )?,
            0,
        )
        .await?;

        assert!(matches!(rx.recv().await?, AsyncSinkReply::CB(_, Trigger)));

        Ok(())
    }

    #[test]
    fn fails_if_the_event_is_not_an_object() -> Result<()> {
        let now = tremor_common::time::nanotime();
        let mut timestamp = Timestamp {
            seconds: (now / 1_000_000_000u64) as i64,
            nanos: (now % 1_000_000_000) as i32,
        };
        timestamp.normalize();
        let data = &literal!("snot");
        let config = Config::new(&literal!({}))?;
        let meta = literal!({});
        let meta = meta.get("gcl_writer").or(None);

        let result = value_to_log_entry(timestamp, &config, data, meta);
        if let Err(Error(ErrorKind::GclTypeMismatch("Value::Object", x), _)) = result {
            assert_eq!(x, ValueType::String);
            Ok(())
        } else {
            Err("Mapping did not fail on non-object event".into())
        }
    }

    #[async_std::test]
    async fn sink_succeeds_if_config_is_empty() -> Result<()> {
        let config = literal!({
            "config": {}
        });

        let result =
            ConnectorHarness::new(function_name!(), &gcl::writer::Builder::default(), &config)
                .await;

        assert!(result.is_ok());

        Ok(())
    }

    #[async_std::test]
    async fn on_event_fails_if_client_is_not_connected() -> Result<()> {
        let (rx, _tx) = async_std::channel::unbounded();
        let (reply_tx, _reply_rx) = async_std::channel::unbounded();
        let config = Config::new(&literal!({
            "connect_timeout": 1_000_000
        }))?;

        let mut sink = GclSink::<TestTokenProvider, _>::new(config, reply_tx, MockChannelFactory);

        let result = sink
            .on_event(
                "",
                Event::signal_tick(),
                &SinkContext {
                    uid: SinkId::default(),
                    alias: Alias::new("", ""),
                    connector_type: ConnectorType::default(),
                    quiescence_beacon: QuiescenceBeacon::default(),
                    notifier: ConnectionLostNotifier::new(rx),
                },
                &mut EventSerializer::new(
                    None,
                    CodecReq::Structured,
                    vec![],
                    &ConnectorType::from(""),
                    &Alias::new("", ""),
                )?,
                0,
            )
            .await;

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn log_name_override() -> Result<()> {
        let now = tremor_common::time::nanotime();
        let mut timestamp = Timestamp {
            seconds: now as i64 / 1_000_000_000i64,
            nanos: (now % 1_000_000_000) as i32,
        };
        timestamp.normalize();
        let config: Config = structurize(literal!({ "log_name": "snot" }))?;
        let data = literal!({"snot": "badger"});
        let meta = literal!({"log_name": "override"});
        let le = value_to_log_entry(timestamp, &config, &data, Some(&meta))?;
        assert_eq!("override", &le.log_name);

        Ok(())
    }

    #[test]
    fn log_severity_override() -> Result<()> {
        let now = tremor_common::time::nanotime();
        let mut timestamp = Timestamp {
            seconds: now as i64 / 1_000_000_000i64,
            nanos: (now % 1_000_000_000) as i32,
        };
        timestamp.normalize();
        let config: Config = structurize(literal!({}))?;
        let data = literal!({"snot": "badger"});
        let meta = literal!({"log_name": "override", "log_severity": LogSeverity::Debug as i32});
        let le = value_to_log_entry(timestamp, &config, &data, Some(&meta))?;
        assert_eq!("override", &le.log_name);
        assert_eq!(LogSeverity::Debug as i32, le.severity);

        Ok(())
    }
}
