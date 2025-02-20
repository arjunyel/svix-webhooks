use std::{sync::Arc, time::Duration};

use axum::async_trait;
use chrono::{DateTime, Utc};
use lapin::options::{BasicAckOptions, BasicNackOptions};
use omniqueue::{
    backends::memory_queue::MemoryQueueBackend,
    queue::{
        consumer::{DynConsumer, QueueConsumer},
        producer::QueueProducer,
        Delivery, QueueBackend as _,
    },
    scheduled::ScheduledProducer,
};
use serde::{Deserialize, Serialize};
use svix_ksuid::*;

use crate::error::Traceable;
use crate::{
    cfg::{Configuration, QueueBackend},
    core::{
        retry::{run_with_retries, Retry},
        types::{ApplicationId, EndpointId, MessageAttemptTriggerType, MessageId},
    },
    error::{Error, ErrorType, Result},
};

use self::redis::{RedisQueueConsumer, RedisQueueInner, RedisQueueProducer};

pub mod rabbitmq;
pub mod redis;

const RETRY_SCHEDULE: &[Duration] = &[
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(40),
];

fn should_retry(err: &Error) -> bool {
    matches!(err.typ, ErrorType::Queue(_))
}

pub async fn new_pair(
    cfg: &Configuration,
    prefix: Option<&str>,
) -> (TaskQueueProducer, TaskQueueConsumer) {
    match cfg.queue_backend() {
        QueueBackend::Redis(dsn) => {
            let pool = crate::redis::new_redis_pool(dsn, cfg).await;
            redis::new_pair(pool, prefix).await
        }
        QueueBackend::RedisCluster(dsn) => {
            let pool = crate::redis::new_redis_pool_clustered(dsn, cfg).await;
            redis::new_pair(pool, prefix).await
        }
        QueueBackend::Memory => {
            let (producer, consumer) = MemoryQueueBackend::builder(())
                .build_pair()
                .await
                .expect("building in-memory queue can't fail");

            (
                TaskQueueProducer::Omni(Arc::new(producer.into_dyn_scheduled(Default::default()))),
                TaskQueueConsumer::Omni(consumer.into_dyn(Default::default())),
            )
        }
        QueueBackend::RabbitMq(dsn) => {
            let prefix = prefix.unwrap_or("");
            let queue = format!("{prefix}-message-queue");
            // Default to a prefetch_size of 1, as it's the safest (least likely to starve consumers)
            let prefetch_size = cfg.rabbit_consumer_prefetch_size.unwrap_or(1);
            rabbitmq::new_pair(dsn, queue, prefetch_size)
                .await
                .expect("can't connect to rabbit")
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageTask {
    pub msg_id: MessageId,
    pub app_id: ApplicationId,
    pub endpoint_id: EndpointId,
    pub trigger_type: MessageAttemptTriggerType,
    pub attempt_count: u16,
}

impl MessageTask {
    pub fn new_task(
        msg_id: MessageId,
        app_id: ApplicationId,
        endpoint_id: EndpointId,
        trigger_type: MessageAttemptTriggerType,
    ) -> QueueTask {
        QueueTask::MessageV1(Self {
            msg_id,
            app_id,
            endpoint_id,
            attempt_count: 0,
            trigger_type,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageTaskBatch {
    pub msg_id: MessageId,
    pub app_id: ApplicationId,
    pub force_endpoint: Option<EndpointId>,
    pub trigger_type: MessageAttemptTriggerType,
}

impl MessageTaskBatch {
    pub fn new_task(
        msg_id: MessageId,
        app_id: ApplicationId,
        force_endpoint: Option<EndpointId>,
        trigger_type: MessageAttemptTriggerType,
    ) -> QueueTask {
        QueueTask::MessageBatch(Self {
            msg_id,
            app_id,
            force_endpoint,
            trigger_type,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type")]
pub enum QueueTask {
    HealthCheck,
    MessageV1(MessageTask),
    MessageBatch(MessageTaskBatch),
}

impl QueueTask {
    /// Returns a type string, for logging.
    pub fn task_type(&self) -> &'static str {
        match self {
            QueueTask::HealthCheck => "HealthCheck",
            QueueTask::MessageV1(_) => "MessageV1",
            QueueTask::MessageBatch(_) => "MessageBatch",
        }
    }
}

#[derive(Clone)]
pub enum TaskQueueProducer {
    Redis(RedisQueueProducer),
    RabbitMq(rabbitmq::Producer),
    Omni(Arc<omniqueue::scheduled::DynScheduledProducer>),
}

impl TaskQueueProducer {
    pub async fn send(&self, task: QueueTask, delay: Option<Duration>) -> Result<()> {
        let task = Arc::new(task);
        run_with_retries(
            || async {
                match self {
                    TaskQueueProducer::Redis(q) => q.send(task.clone(), delay).await,
                    TaskQueueProducer::RabbitMq(q) => q.send(task.clone(), delay).await,
                    TaskQueueProducer::Omni(q) => if let Some(delay) = delay {
                        q.send_serde_json_scheduled(task.as_ref(), delay).await
                    } else {
                        q.send_serde_json(task.as_ref()).await
                    }
                    .map_err(Into::into),
                }
            },
            should_retry,
            RETRY_SCHEDULE,
        )
        .await
    }
}

pub enum TaskQueueConsumer {
    Redis(RedisQueueConsumer),
    RabbitMq(rabbitmq::Consumer),
    Omni(DynConsumer),
}

impl TaskQueueConsumer {
    pub async fn receive_all(&mut self) -> Result<Vec<TaskQueueDelivery>> {
        match self {
            TaskQueueConsumer::Redis(q) => q.receive_all().await.trace(),
            TaskQueueConsumer::RabbitMq(q) => q.receive_all().await.trace(),
            TaskQueueConsumer::Omni(q) => {
                const MAX_MESSAGES: usize = 128;
                // FIXME(onelson): need to figure out what deadline/duration to use here
                q.receive_all(MAX_MESSAGES, Duration::from_secs(30))
                    .await
                    .map_err(Into::into)
                    .trace()?
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect()
            }
        }
    }
}

/// Used by TaskQueueDeliveries to Ack/Nack itself
#[derive(Debug)]
enum Acker {
    Redis(Arc<RedisQueueInner>),
    RabbitMQ(lapin::message::Delivery),
    Omni(Delivery),
}

#[derive(Debug)]
pub struct TaskQueueDelivery {
    pub id: String,
    pub task: Arc<QueueTask>,
    acker: Acker,
}

impl TaskQueueDelivery {
    /// The `timestamp` is when this message will be delivered at
    fn from_arc(task: Arc<QueueTask>, timestamp: Option<DateTime<Utc>>, acker: Acker) -> Self {
        let ksuid = KsuidMs::new(timestamp, None);
        Self {
            id: ksuid.to_string(),
            task,
            acker,
        }
    }

    pub async fn ack(self) -> Result<()> {
        tracing::trace!("ack {}", self.id);

        let mut retry = Retry::new(should_retry, RETRY_SCHEDULE);
        let mut acker = Some(self.acker);
        loop {
            if let Some(result) = retry
                .run(|| async {
                    let acker_ref = acker
                        .as_ref()
                        .expect("acker is always Some when trying to ack");
                    match acker_ref {
                        Acker::Redis(q) => q.ack(&self.id, &self.task).await.trace(),
                        Acker::RabbitMQ(delivery) => {
                            delivery
                                .ack(BasicAckOptions {
                                    multiple: false, // Only ack this message, not others
                                })
                                .await
                                .map_err(Into::into)
                        }
                        Acker::Omni(_) => match acker.take() {
                            Some(Acker::Omni(delivery)) => {
                                delivery.ack().await.map_err(|(e, delivery)| {
                                    // Put the delivery back in acker beforr retrying, to
                                    // satisfy the expect above.
                                    acker = Some(Acker::Omni(delivery));
                                    e.into()
                                })
                            }
                            _ => unreachable!(),
                        },
                    }
                })
                .await
            {
                return result;
            }
        }
    }

    pub async fn nack(self) -> Result<()> {
        tracing::trace!("nack {}", self.id);

        let mut retry = Retry::new(should_retry, RETRY_SCHEDULE);
        let mut acker = Some(self.acker);
        loop {
            if let Some(result) = retry
                .run(|| async {
                    let acker_ref = acker
                        .as_ref()
                        .expect("acker is always Some when trying to ack");
                    match acker_ref {
                        Acker::Redis(q) => q.nack(&self.id, &self.task).await.trace(),
                        Acker::RabbitMQ(delivery) => {
                            // See https://www.rabbitmq.com/confirms.html#consumer-nacks-requeue

                            delivery
                                .nack(BasicNackOptions {
                                    requeue: true,
                                    multiple: false, // Only nack this message, not others
                                })
                                .await
                                .map_err(Into::into)
                        }
                        Acker::Omni(_) => match acker.take() {
                            Some(Acker::Omni(delivery)) => {
                                delivery
                                    .nack()
                                    .await
                                    .map_err(|(e, delivery)| {
                                        // Put the delivery back in acker beforr retrying, to
                                        // satisfy the expect above.
                                        acker = Some(Acker::Omni(delivery));
                                        e.into()
                                    })
                                    .trace()
                            }
                            _ => unreachable!(),
                        },
                    }
                })
                .await
            {
                return result;
            }
        }
    }
}

impl TryFrom<Delivery> for TaskQueueDelivery {
    type Error = Error;
    fn try_from(value: Delivery) -> Result<Self> {
        Ok(TaskQueueDelivery {
            // FIXME(onelson): ksuid for the id?
            //   Since ack/nack is all handled internally by the omniqueue delivery, maybe it
            //   doesn't matter.
            id: "".to_string(),
            task: Arc::new(
                value
                    .payload_serde_json()
                    .map_err(|_| Error::queue("Failed to decode queue task"))?
                    .ok_or_else(|| Error::queue("Unexpected empty delivery"))?,
            ),
            acker: Acker::Omni(value),
        })
    }
}

#[async_trait]
trait TaskQueueSend: Sync + Send {
    async fn send(&self, task: Arc<QueueTask>, delay: Option<Duration>) -> Result<()>;
}

#[async_trait]
trait TaskQueueReceive {
    async fn receive_all(&mut self) -> Result<Vec<TaskQueueDelivery>>;
}
