// SPDX-License-Identifier: CC0-1.0

//! An EC2 compute instance product carries the `marketoption` attribute AWS publishes on
//! it. OpenCost prices a node only from a product whose `marketoption` is `OnDemand`
//! (`pkg/cloud/aws/provider.go`, `populatePricing`), so without it every node falls back
//! to OpenCost's default CPU and memory prices.

use serde_json::json;

use super::pricing_support::{offer_attribute, offer_documents, ok, router};

#[tokio::test]
async fn an_on_demand_compute_instance_is_found_by_its_market_option() {
    let router = router();

    let response = ok(
        &router,
        "GetProducts",
        &json!({
            "ServiceCode": "AmazonEC2",
            "Filters": [
                {"Type": "TERM_MATCH", "Field": "instanceType", "Value": "t3.medium"},
                {"Type": "TERM_MATCH", "Field": "regionCode", "Value": "us-east-1"},
                {"Type": "TERM_MATCH", "Field": "operatingSystem", "Value": "Linux"},
                {"Type": "TERM_MATCH", "Field": "tenancy", "Value": "Shared"},
                {"Type": "TERM_MATCH", "Field": "capacitystatus", "Value": "Used"},
                {"Type": "TERM_MATCH", "Field": "preInstalledSw", "Value": "NA"},
                {"Type": "TERM_MATCH", "Field": "marketoption", "Value": "OnDemand"},
            ],
            "MaxResults": 100,
        }),
    )
    .await;

    let documents = offer_documents(&response);
    assert_eq!(documents.len(), 1, "{response}");
    assert_eq!(offer_attribute(&documents[0], "marketoption").as_deref(), Some("OnDemand"));
}
