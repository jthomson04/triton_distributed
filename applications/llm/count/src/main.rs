// SPDX-FileCopyrightText: Copyright (c) 2024-2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
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

//! Count is a metrics aggregator designed to operate within a namespace and collect
//! metrics from all workers.
//!
//! Metrics will collect for now:
//!
//! - LLM Worker Load:Capacity
//!   - These metrics will be scraped by the LLM NATS Service API's stats request
//!   - Request Slots: [Active, Total]
//!   - KV Cache Blocks: [Active, Total]

use clap::Parser;
use serde::{Deserialize, Serialize};

use triton_distributed_runtime::{
    error, logging,
    traits::events::EventPublisher,
    utils::{Duration, Instant},
    DistributedRuntime, ErrorContext, Result, Runtime, Worker,
};

/// CLI arguments for the count application
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Component to scrape metrics from
    #[arg(long)]
    component: String,

    /// Endpoint to scrape metrics from
    #[arg(long)]
    endpoint: String,

    /// Namespace to operate in
    #[arg(long, env = "TRD_NAMESPACE", default_value = "triton-init")]
    namespace: String,

    /// Polling interval in seconds (minimum 1 second)
    #[arg(long, default_value = "2")]
    poll_interval: u64,
}

fn get_config(args: &Args) -> Result<LLMWorkerLoadCapacityConfig> {
    if args.component.is_empty() {
        return Err(error!("Component name cannot be empty"));
    }

    if args.endpoint.is_empty() {
        return Err(error!("Endpoint name cannot be empty"));
    }

    if args.poll_interval < 1 {
        return Err(error!("Polling interval must be at least 1 second"));
    }

    Ok(LLMWorkerLoadCapacityConfig {
        component_name: args.component.clone(),
        endpoint_name: args.endpoint.clone(),
    })
}

// we will scrape the service_name and extract the endpoint_name metrics
// we will bcast them as {namespace}.events.l2c.{service_name}.{endpoint_name}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMWorkerLoadCapacityConfig {
    component_name: String,
    endpoint_name: String,
}

/// LLM Worker Load Capacity Metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMWorkerLoadCapacity {
    pub requests_active_slots: u32,
    pub requests_total_slots: u32,
    pub kv_blocks_active: u32,
    pub kv_blocks_total: u32,
}

fn main() -> Result<()> {
    logging::init();
    let worker = Worker::from_settings()?;
    worker.execute(app)
}

// TODO - refactor much of this back into the library
async fn app(runtime: Runtime) -> Result<()> {
    let args = Args::parse();
    // we will start by assuming that there is no oscar and no planner
    // to that end, we will use CLI args to get a singular config for scraping a single backend
    let config = get_config(&args)?;
    tracing::info!("Config: {config:?}");

    let drt = DistributedRuntime::from_settings(runtime.clone()).await?;

    let namespace = drt.namespace(args.namespace)?;
    let component = namespace.component("count")?;

    // there should only be one count
    // check {component.etcd_path()}/instance for existing instances
    let key = format!("{}/instance", component.etcd_path());
    tracing::info!("Creating unique instance of Count at {key}");
    drt.etcd_client()
        .kv_create(
            key,
            serde_json::to_vec_pretty(&config)?,
            Some(drt.primary_lease().id()),
        )
        .await
        .context("Unable to create unique instance of Count; possibly one already exists")?;

    let target = namespace.component(&config.component_name)?;
    let target_endpoint = target.endpoint(&config.endpoint_name);

    let service_name = target.service_name();
    let service_subject = target_endpoint.subject();
    tracing::info!("Scraping service {service_name} and filtering on subject {service_subject}");

    let token = drt.primary_lease().child_token();

    let address = format!("{}.{}", config.component_name, config.endpoint_name,);
    let event_name = format!("l2c.{}", address);

    loop {
        let next = Instant::now() + Duration::from_secs(args.poll_interval);

        // collect stats from each backend
        let stream = target.scrape_stats(Duration::from_secs(1)).await?;
        tracing::debug!("Scraped Stats Stream: {stream:?}");

        // filter the stats by the service subject
        let endpoints = stream
            .into_endpoints()
            .filter(|e| e.subject.starts_with(&service_subject))
            .collect::<Vec<_>>();

        tracing::debug!("Endpoints: {endpoints:?}");
        if endpoints.is_empty() {
            tracing::warn!("No endpoints found matching subject {}", service_subject);
        }

        // extract the custom data from the stats and try to decode it as LLMWorkerLoadCapacity
        let metrics = endpoints
            .iter()
            .filter_map(|e| match e.data.clone() {
                Some(metrics) => metrics.decode::<LLMWorkerLoadCapacity>().ok(),
                None => None,
            })
            .collect::<Vec<_>>();
        tracing::debug!("Metrics: {metrics:?}");

        // parse the endpoint ids
        // the ids are the last part of the subject in hexadecimal
        // form a list of tuples (kv_blocks_total - kv_blocks_active, requests_total_slots - requests_active_slots, id)
        // this tuple represent the remaining capacity of each endpoint
        let capacity_with_ids = metrics
            .iter()
            .zip(endpoints.iter())
            .filter_map(|(m, e)| {
                e.id().ok().map(|id| {
                    (
                        m.kv_blocks_total - m.kv_blocks_active,
                        m.requests_total_slots - m.requests_active_slots,
                        id,
                    )
                })
            })
            .collect::<Vec<_>>();

        // compute mean / std of LLMWorkerLoadCapacity
        let load_values: Vec<f64> = metrics.iter().map(|x| x.kv_blocks_active as f64).collect();
        let load_avg = load_values.iter().sum::<f64>() / load_values.len() as f64;
        let variance = load_values
            .iter()
            .map(|&x| (x - load_avg).powi(2))
            .sum::<f64>()
            / load_values.len() as f64;
        let load_std = variance.sqrt();

        let processed = ProcessedEndpoints {
            capacity_with_ids,
            load_avg,
            load_std,
            address: address.clone(),
        };

        // publish using the namespace event plane
        tracing::info!(
            "Publishing event {event_name} on namespace {namespace:?} with {processed:?}"
        );
        namespace.publish(&event_name, &processed).await?;

        // wait until cancelled or the next tick
        match tokio::time::timeout_at(next, token.cancelled()).await {
            Ok(_) => break,
            Err(_) => {
                // timeout, we continue
                continue;
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessedEndpoints {
    /// (kv_blocks_total - kv_blocks_active, requests_total_slots - requests_active_slots, id)
    pub capacity_with_ids: Vec<(u32, u32, i64)>,

    /// kv_blocks_active average
    pub load_avg: f64,

    /// kv_blocks_active standard deviation
    pub load_std: f64,

    /// {component}.{endpoint}
    pub address: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_namespace_from_env() {
        env::set_var("TRD_NAMESPACE", "test-namespace");

        // Parse args with no explicit namespace
        let args = Args::parse_from(["count", "--component", "comp", "--endpoint", "end"]);

        // Verify namespace was taken from environment variable
        assert_eq!(args.namespace, "test-namespace");
    }
}
