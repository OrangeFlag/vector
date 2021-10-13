use crate::conditions::{AnyCondition, Condition};
use crate::config::{DataType, TransformConfig, TransformContext, TransformDescription};
use crate::event::Event;
use crate::transforms::{TaskTransform, Transform};

use async_stream::stream;
use futures::{stream, Stream, StreamExt};
use governor::*;
use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::time::Duration;

#[derive(Deserialize, Default, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct ThrottleConfig {
    threshold: u32,
    window: f64,
    key_field: Option<String>,
    exclude: Option<AnyCondition>,
}

inventory::submit! {
    TransformDescription::new::<ThrottleConfig>("throttle")
}

impl_generate_config_from_default!(ThrottleConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "throttle")]
impl TransformConfig for ThrottleConfig {
    async fn build(&self, context: &TransformContext) -> crate::Result<Transform> {
        Throttle::new(self, context).map(Transform::task)
    }

    fn input_type(&self) -> DataType {
        DataType::Log
    }

    fn output_type(&self) -> DataType {
        DataType::Log
    }

    fn transform_type(&self) -> &'static str {
        "throttle"
    }
}

#[derive(Clone)]
pub struct Throttle {
    quota: Quota,
    key_field: Option<String>,
    exclude: Option<Box<dyn Condition>>,
}

impl Throttle {
    pub fn new(config: &ThrottleConfig, context: &TransformContext) -> crate::Result<Self> {
        let threshold = match NonZeroU32::new(config.threshold) {
            Some(threshold) => threshold,
            None => return Err(Box::new(ConfigError::NonZero)),
        };

        let quota = match Quota::with_period(Duration::from_secs_f64(
            config.window / threshold.get() as f64,
        )) {
            Some(quota) => quota.allow_burst(threshold),
            None => return Err(Box::new(ConfigError::NonZero)),
        };
        let exclude = config
            .exclude
            .as_ref()
            .map(|condition| condition.build(&context.enrichment_tables))
            .transpose()?;

        Ok(Self {
            quota,
            key_field: None,
            exclude,
        })
    }
}

impl TaskTransform for Throttle {
    fn transform(
        self: Box<Self>,
        mut input_rx: Pin<Box<dyn Stream<Item = Event> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = Event> + Send>>
    where
        Self: 'static,
    {
        let limiter = RateLimiter::keyed(self.quota);

        let mut flush_stream = tokio::time::interval(Duration::from_millis(1000));

        Box::pin(
            stream! {
              loop {
                let mut output = Vec::new();
                let done = tokio::select! {
                    _ = flush_stream.tick() => {
                        false
                    }
                    maybe_event = input_rx.next() => {
                        match maybe_event {
                            None => true,
                            Some(event) => {
                                if let Some(condition) = self.exclude.as_ref() {
                                    if condition.check(&event) {
                                        output.push(event);
                                    } else {
                                        match limiter.check_key(&"default") {
                                            Ok(()) => {
                                                output.push(event);
                                            }
                                            _ => {
                                                // Dropping event
                                            }
                                        }
                                    }
                                } else {
                                    match limiter.check_key(&"default") {
                                        Ok(()) => {
                                            output.push(event);
                                        }
                                        _ => {
                                            // Dropping event
                                        }
                                    }
                                }
                                false
                            }
                        }
                    }
                };
                yield stream::iter(output.into_iter());
                if done { break }
              }
            }
            .flatten(),
        )
    }
}

#[derive(Debug, Snafu)]
pub enum ConfigError {
    #[snafu(display("`threshold`, and `window` must be non-zero"))]
    NonZero,
}
