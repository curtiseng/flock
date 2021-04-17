// Copyright 2020 UMD Database Group. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The generic lambda function for sub-plan execution on AWS Lambda.
use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use datafusion::physical_plan::Partitioning;
use futures::executor::block_on;
use lambda_runtime::{handler_fn, Context};
use lazy_static::lazy_static;
use log::warn;
use nexmark::event::{Auction, Bid, Person};
use nexmark::{NexMarkEvent, NexMarkSource};
use rayon::prelude::*;
use runtime::prelude::*;
use rusoto_core::Region;
use rusoto_lambda::{InvokeAsyncRequest, Lambda, LambdaClient};
use serde_json::Value;
use std::cell::Cell;
use std::sync::Arc;
use std::sync::Once;

#[cfg(feature = "snmalloc")]
#[global_allocator]
static ALLOC: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

/// Initializes the lambda function once and only once.
static INIT: Once = Once::new();

thread_local! {
    /// Is in the testing environment.
    static IS_TESTING: Cell<bool> = Cell::new(false);
}

lazy_static! {
    static ref PERSON_SCHEMA: SchemaRef = Arc::new(Person::schema());
    static ref AUCTION_SCHEMA: SchemaRef = Arc::new(Auction::schema());
    static ref BID_SCHEMA: SchemaRef = Arc::new(Bid::schema());
}

/// A wrapper to allow the declaration of the execution context of the lambda
/// function.
enum CloudFunctionContext {
    Lambda((Box<ExecutionContext>, Arena)),
    Uninitialized,
}

/// Lambda execution context.
static mut EXECUTION_CONTEXT: CloudFunctionContext = CloudFunctionContext::Uninitialized;

/// Performs an initialization routine once and only once.
macro_rules! init_exec_context {
    () => {{
        unsafe {
            // Init query executor from the cloud evironment.
            let init_context = || match std::env::var(&globals["lambda"]["name"]) {
                Ok(s) => {
                    EXECUTION_CONTEXT = CloudFunctionContext::Lambda((
                        Box::new(ExecutionContext::unmarshal(&s)),
                        Arena::new(),
                    ));
                }
                Err(_) => {
                    panic!("No execution context in the cloud environment.");
                }
            };
            if IS_TESTING.with(|t| t.get()) {
                init_context();
            } else {
                INIT.call_once(init_context);
            }
            match &mut EXECUTION_CONTEXT {
                CloudFunctionContext::Lambda((ctx, arena)) => (ctx, arena),
                CloudFunctionContext::Uninitialized => panic!("Uninitialized execution context!"),
            }
        }
    }};
}

/// Invoke functions in the next stage of the data flow.
fn invoke_next_functions(ctx: &ExecutionContext, batches: &mut Vec<RecordBatch>) -> Result<()> {
    // retrieve the next lambda function names
    let next_func = LambdaExecutor::next_function(&ctx)?;

    // create uuid builder to assign id to each payload
    let uuid_builder = UuidBuilder::new(&ctx.name, batches.len());

    let client = &LambdaClient::new(Region::default());
    batches.into_par_iter().enumerate().for_each(|(i, batch)| {
        // call the lambda function asynchronously until it succeeds.
        loop {
            let uuid = uuid_builder.get(i);
            let request = InvokeAsyncRequest {
                function_name: next_func.clone(),
                invoke_args:   Payload::to_bytes(&batch, uuid, Encoding::default()),
            };

            if let Ok(reponse) = block_on(client.invoke_async(request)) {
                if let Some(code) = reponse.status {
                    // A success response (202 Accepted) indicates that the request
                    // is queued for invocation.
                    if code == 202 {
                        break;
                    } else {
                        warn!("Unknown invoke error: {}, retry ... ", code);
                    }
                }
            }
        }
    });

    Ok(())
}

async fn payload_handler(
    ctx: &mut ExecutionContext,
    arena: &mut Arena,
    event: Value,
) -> Result<Value> {
    let input_partitions = {
        if match &ctx.next {
            CloudFunction::None | CloudFunction::Solo(..) => true,
            CloudFunction::Chorus(..) => false,
        } {
            // ressemble lambda n to 1
            let (ready, uuid) = arena.reassemble(event);
            if ready {
                arena.batches(uuid.tid)
            } else {
                return Err(SquirtleError::Execution(
                    "window data collection has not been completed.".to_string(),
                ));
            }
        } else {
            // partition lambda 1 to n
            let (batch, _) = Payload::to_batch(event);
            vec![batch]
        }
    };

    if input_partitions.is_empty() || input_partitions[0].is_empty() {
        return Err(SquirtleError::Execution(
            "payload data is empty.".to_string(),
        ));
    }

    // TODO(gangliao): repartition input batches to speedup the operations.
    ctx.feed_one_source(&input_partitions);
    let output_partitions = ctx.execute().await?;

    if ctx.next != CloudFunction::None {
        let mut batches = LambdaExecutor::coalesce_batches(
            vec![output_partitions],
            globals["lambda"]["payload_batch_size"]
                .parse::<usize>()
                .unwrap(),
        )
        .await?;
        assert_eq!(1, batches.len());
        // call the next stage of the dataflow graph.
        invoke_next_functions(&ctx, &mut batches[0])?;
    }

    // TODO(gangliao): sink results to other cloud services.
    Ok(serde_json::to_value(&ctx.name)?)
}

async fn nexmark_bench_handler(ctx: &mut ExecutionContext, event: Value) -> Result<Value> {
    if let DataSource::NexMarkEvent(source) = &ctx.datasource {
        match source.window {
            StreamWindow::TumblingWindow(Schedule::Seconds(_sec)) => {
                unimplemented!();
            }
            StreamWindow::HoppingWindow((_window, _hop))
            | StreamWindow::SlidingWindow((_window, _hop)) => {
                unimplemented!();
            }
            StreamWindow::None => {
                // data sink -- /dev/null
                collect(ctx, event).await?;
            }
            _ => unimplemented!(),
        }
    }

    Ok(serde_json::to_value(&ctx.name)?)
}

async fn handler(event: Value, _: Context) -> Result<Value> {
    let (mut ctx, mut arena) = init_exec_context!();

    match &ctx.datasource {
        DataSource::Payload => payload_handler(&mut ctx, &mut arena, event).await,
        DataSource::NexMarkEvent(_) => nexmark_bench_handler(&mut ctx, event).await,
        _ => unimplemented!(),
    }
}

async fn feed_one_source(ctx: &mut ExecutionContext, batches: Vec<RecordBatch>) -> Result<()> {
    let num_batches = batches.len();
    let parallelism = globals["lambda"]["parallelism"].parse::<usize>().unwrap();

    if num_batches > parallelism {
        ctx.feed_one_source(
            &LambdaExecutor::repartition(vec![batches], Partitioning::RoundRobinBatch(parallelism))
                .await?,
        );
    } else if num_batches > 1 {
        ctx.feed_one_source(
            &LambdaExecutor::repartition(vec![batches], Partitioning::RoundRobinBatch(num_batches))
                .await?,
        );
    } else {
        // only one batch exists
        assert!(num_batches == 1);
        ctx.feed_one_source(&vec![batches]);
    }

    Ok(())
}

async fn collect(ctx: &mut ExecutionContext, value: Value) -> Result<Vec<RecordBatch>> {
    let event: NexMarkEvent = serde_json::from_value(value).unwrap();
    let person_batches = NexMarkSource::to_batch(&event.persons, PERSON_SCHEMA.clone());
    let auction_batches = NexMarkSource::to_batch(&event.auctions, AUCTION_SCHEMA.clone());
    let bid_batches = NexMarkSource::to_batch(&event.bids, BID_SCHEMA.clone());

    if person_batches.is_empty() && auction_batches.is_empty() && bid_batches.is_empty() {
        return Err(SquirtleError::Execution("No Nexmark input!".to_owned()));
    }

    match ctx.query_number {
        Some(0) | Some(1) | Some(2) => feed_one_source(ctx, bid_batches).await?,
        _ => unimplemented!(),
    }

    // query execution
    let output_partitions = ctx.execute().await?;

    // show output
    let formatted = arrow::util::pretty::pretty_format_batches(&output_partitions).unwrap();
    println!("{}", formatted);

    Ok(output_partitions)
}

#[tokio::main]
async fn main() -> Result<()> {
    lambda_runtime::run(handler_fn(handler)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexmark::config::Config;
    use rusoto_core::Region;
    use rusoto_lambda::{InvocationRequest, Lambda, LambdaClient};

    #[tokio::test]
    #[ignore]
    async fn q1() -> Result<()> {
        let mut config = Config::new();
        config.insert("threads", 10.to_string());
        config.insert("seconds", 1.to_string());
        config.insert("events-per-second", 100.to_string());
        let nex = NexMarkSource {
            config,
            ..Default::default()
        };
        let events = nex.generate_data()?;
        let events = events.select(0, 1).unwrap();

        // serialization
        let en_events = serde_json::to_vec(&events).unwrap();

        let client = LambdaClient::new(Region::UsEast1);
        let request = InvocationRequest {
            function_name: "arn:aws:lambda:us-east-1:942368842860:function:umd_runtime".to_string(),
            payload: Some(en_events.into()),
            invocation_type: Some("RequestResponse".to_string()),
            ..Default::default()
        };
        let response = client.invoke(request).await.unwrap();

        // deserialization
        let de_events: String = serde_json::from_slice(&response.payload.unwrap()).unwrap();
        println!("{}", de_events);

        Ok(())
    }
}