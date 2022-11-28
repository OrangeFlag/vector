use std::sync::Arc;
use std::collections::BTreeMap;

use bytes::{BufMut, Bytes, BytesMut};
use chrono::Utc;
use codecs::StreamDecodingError;
use http::StatusCode;
use lookup::path;
use tokio_util::codec::Decoder;
use value::Value;
use vector_core::ByteSizeOf;
use warp::{filters::BoxedFilter, path as warp_path, path::FullPath, reply::Response, Filter};

use crate::{
    event::Event,
    internal_events::EventsReceived,
    sources::{
        datadog_agent::{handle_request, ApiKeyQueryParams, DatadogAgentSource, LogMsg},
        util::ErrorMessage,
    },
    SourceSender,
};

pub(crate) fn build_warp_filter(
    acknowledgements: bool,
    multiple_outputs: bool,
    out: SourceSender,
    source: DatadogAgentSource,
    standardize_tags: bool,
) -> BoxedFilter<(Response,)> {
    warp::post()
        .and(warp_path!("v1" / "input" / ..).or(warp_path!("api" / "v2" / "logs" / ..)))
        .and(warp::path::full())
        .and(warp::header::optional::<String>("content-encoding"))
        .and(warp::header::optional::<String>("dd-api-key"))
        .and(warp::query::<ApiKeyQueryParams>())
        .and(warp::body::bytes())
        .and_then(
            move |_,
                  path: FullPath,
                  encoding_header: Option<String>,
                  api_token: Option<String>,
                  query_params: ApiKeyQueryParams,
                  body: Bytes| {
                let events = source
                    .decode(&encoding_header, body, path.as_str())
                    .and_then(|body| {
                        decode_log_body(
                            body,
                            source.api_key_extractor.extract(
                                path.as_str(),
                                api_token,
                                query_params.dd_api_key,
                            ),
                            &source,
                            standardize_tags,
                        )
                    });

                if multiple_outputs {
                    handle_request(events, acknowledgements, out.clone(), Some(super::LOGS))
                } else {
                    handle_request(events, acknowledgements, out.clone(), None)
                }
            },
        )
        .boxed()
}

pub(crate) fn decode_log_body(
    body: Bytes,
    api_key: Option<Arc<str>>,
    source: &DatadogAgentSource,
    simplify_log_tags: bool,
) -> Result<Vec<Event>, ErrorMessage> {
    if body.is_empty() {
        // The datadog agent may send an empty payload as a keep alive
        debug!(
            message = "Empty payload ignored.",
            internal_log_rate_limit = true
        );
        return Ok(Vec::new());
    }

    let messages: Vec<LogMsg> = serde_json::from_slice(&body).map_err(|error| {
        ErrorMessage::new(
            StatusCode::BAD_REQUEST,
            format!("Error parsing JSON: {:?}", error),
        )
    })?;

    let now = Utc::now();
    let mut decoded = Vec::new();

    for LogMsg {
        message,
        status,
        timestamp,
        hostname,
        service,
        ddsource,
        ddtags,
    } in messages
    {
        let mut decoder = source.decoder.clone();
        let mut buffer = BytesMut::new();
        buffer.put(message);
        loop {
            match decoder.decode_eof(&mut buffer) {
                Ok(Some((events, _byte_size))) => {
                    for mut event in events {
                        if let Event::Log(ref mut log) = event {
                            let namespace = &source.log_namespace;
                            let source_name = "datadog_agent";

                            namespace.insert_source_metadata(
                                source_name,
                                log,
                                path!("status"),
                                path!("status"),
                                status.clone(),
                            );
                            namespace.insert_source_metadata(
                                source_name,
                                log,
                                path!("timestamp"),
                                path!("timestamp"),
                                timestamp,
                            );
                            namespace.insert_source_metadata(
                                source_name,
                                log,
                                path!("hostname"),
                                path!("hostname"),
                                hostname.clone(),
                            );
                            namespace.insert_source_metadata(
                                source_name,
                                log,
                                path!("service"),
                                path!("service"),
                                service.clone(),
                            );
                            namespace.insert_source_metadata(
                                source_name,
                                log,
                                path!("ddsource"),
                                path!("ddsource"),
                                ddsource.clone(),
                            );

                            if !simplify_log_tags {
                                namespace.insert_source_metadata(
                                    source_name,
                                    log,
                                    path!("ddtags"),
                                    path!("ddtags"),
                                    ddtags.clone(),
                                );
                            } else {
                                let ts = String::from_utf8(ddtags.to_vec()).unwrap();
                                let s = ts.split(',');
                                let mut tags = Value::from(BTreeMap::new());

                                for each in s {
                                    // The expected format is a comma-delimited list of key-value pairs,
                                    // which are themselves separated by colons. If no colon is present,
                                    // then we normalize to a sentinel value. This allows us to
                                    // handle the fact that Datadog tags are not required to have a value.
                                    // e.g., one:yes,two:no,three
                                    let mut kv = each.split(':');
                                    let k = kv.next().unwrap();
                                    match kv.next() {
                                        Some(v) => {
                                            tags.insert(k, v);
                                        }
                                        None => {
                                            tags.insert(k, None::<Value>);
                                        }
                                    }
                                }
                                namespace.insert_source_metadata(
                                    source_name,
                                    log,
                                    path!("tags"),
                                    path!("tags"),
                                    tags.clone(),
                                );

                                // Having the original `ddtags` value in the log event would
                                // just be confusing, so let's remove it. It will be re-assembled
                                // later, at the sink level.
                                log.remove("ddtags");
                            }

                            namespace.insert_vector_metadata(
                                log,
                                path!(source.log_schema_source_type_key),
                                path!("source_type"),
                                Bytes::from("datadog_agent"),
                            );
                            namespace.insert_vector_metadata(
                                log,
                                path!(source.log_schema_timestamp_key),
                                path!("ingest_timestamp"),
                                now,
                            );

                            if let Some(k) = &api_key {
                                log.metadata_mut().set_datadog_api_key(Arc::clone(k));
                            }

                            log.metadata_mut()
                                .set_schema_definition(&source.logs_schema_definition);
                        }

                        decoded.push(event);
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    // Error is logged by `crate::codecs::Decoder`, no further
                    // handling is needed here.
                    if !error.can_continue() {
                        break;
                    }
                }
            }
        }
    }

    emit!(EventsReceived {
        byte_size: decoded.size_of(),
        count: decoded.len(),
    });

    Ok(decoded)
}
