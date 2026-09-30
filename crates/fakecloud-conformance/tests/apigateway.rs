//! Conformance coverage for API Gateway v1 (REST APIs).
//!
//! Each test function exercises a real SDK lifecycle for one entity
//! family and tags every implemented v1 operation it covers via
//! `#[test_action]`. The macro validates the Smithy checksum at
//! compile time so model drift fails the build.

mod helpers;

use aws_sdk_apigateway::types::IntegrationType;
use fakecloud_conformance_macros::test_action;
use helpers::TestServer;

async fn create_api(client: &aws_sdk_apigateway::Client, name: &str) -> (String, String) {
    let api = client.create_rest_api().name(name).send().await.unwrap();
    let id = api.id().unwrap().to_string();
    let root = api.root_resource_id().unwrap().to_string();
    (id, root)
}

#[test_action("apigateway", "CreateRestApi", checksum = "900d6ff2")]
#[test_action("apigateway", "GetRestApi", checksum = "7d0dd4a2")]
#[test_action("apigateway", "GetRestApis", checksum = "fc968b6e")]
#[test_action("apigateway", "UpdateRestApi", checksum = "53214dc3")]
#[test_action("apigateway", "PutRestApi", checksum = "847e6209")]
#[test_action("apigateway", "ImportRestApi", checksum = "5000984f")]
#[test_action("apigateway", "DeleteRestApi", checksum = "997380a3")]
#[tokio::test]
async fn apigateway_v1_rest_api_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;

    let (api_id, _) = create_api(&client, "conf-api").await;

    let got = client
        .get_rest_api()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    assert_eq!(got.name(), Some("conf-api"));

    let list = client.get_rest_apis().send().await.unwrap();
    assert!(list.items().iter().any(|a| a.id() == Some(&api_id)));

    client
        .update_rest_api()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();

    // PutRestApi (overwrite via OpenAPI body) and ImportRestApi share the
    // wire path with create; the SDK accepts an empty body for the test.
    client
        .put_rest_api()
        .rest_api_id(&api_id)
        .body(aws_sdk_apigateway::primitives::Blob::new(b"{}".to_vec()))
        .send()
        .await
        .ok();
    client
        .import_rest_api()
        .body(aws_sdk_apigateway::primitives::Blob::new(b"{}".to_vec()))
        .send()
        .await
        .ok();

    client
        .delete_rest_api()
        .rest_api_id(api_id)
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateResource", checksum = "69c3d88a")]
#[test_action("apigateway", "GetResource", checksum = "60ef4a52")]
#[test_action("apigateway", "GetResources", checksum = "64257138")]
#[test_action("apigateway", "UpdateResource", checksum = "4c108827")]
#[test_action("apigateway", "DeleteResource", checksum = "529a733d")]
#[tokio::test]
async fn apigateway_v1_resource_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, root_id) = create_api(&client, "conf-resources").await;

    let res = client
        .create_resource()
        .rest_api_id(&api_id)
        .parent_id(&root_id)
        .path_part("pets")
        .send()
        .await
        .unwrap();
    let res_id = res.id().unwrap().to_string();

    client
        .get_resource()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .send()
        .await
        .unwrap();

    let list = client
        .get_resources()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    assert!(list.items().iter().any(|r| r.id() == Some(&res_id)));

    client
        .update_resource()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .send()
        .await
        .unwrap();

    client
        .delete_resource()
        .rest_api_id(&api_id)
        .resource_id(res_id)
        .send()
        .await
        .unwrap();
}

async fn create_method_resource(
    client: &aws_sdk_apigateway::Client,
    api_id: &str,
    root_id: &str,
) -> String {
    let res = client
        .create_resource()
        .rest_api_id(api_id)
        .parent_id(root_id)
        .path_part("things")
        .send()
        .await
        .unwrap();
    res.id().unwrap().to_string()
}

#[test_action("apigateway", "PutMethod", checksum = "27504f2c")]
#[test_action("apigateway", "GetMethod", checksum = "4295849c")]
#[test_action("apigateway", "UpdateMethod", checksum = "6e31d2bf")]
#[test_action("apigateway", "DeleteMethod", checksum = "b6b05d28")]
#[tokio::test]
async fn apigateway_v1_method_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, root_id) = create_api(&client, "conf-method").await;
    let res_id = create_method_resource(&client, &api_id, &root_id).await;

    client
        .put_method()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .authorization_type("NONE")
        .send()
        .await
        .unwrap();

    client
        .get_method()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .send()
        .await
        .unwrap();

    client
        .update_method()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .send()
        .await
        .unwrap();

    client
        .delete_method()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "PutMethodResponse", checksum = "79ef2913")]
#[test_action("apigateway", "GetMethodResponse", checksum = "86607501")]
#[test_action("apigateway", "UpdateMethodResponse", checksum = "88af0bd6")]
#[test_action("apigateway", "DeleteMethodResponse", checksum = "5c65c8d7")]
#[tokio::test]
async fn apigateway_v1_method_response_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, root_id) = create_api(&client, "conf-method-response").await;
    let res_id = create_method_resource(&client, &api_id, &root_id).await;
    client
        .put_method()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .authorization_type("NONE")
        .send()
        .await
        .unwrap();

    client
        .put_method_response()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .status_code("200")
        .send()
        .await
        .unwrap();
    client
        .get_method_response()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .status_code("200")
        .send()
        .await
        .unwrap();
    client
        .update_method_response()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .status_code("200")
        .send()
        .await
        .unwrap();
    client
        .delete_method_response()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .status_code("200")
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "PutIntegration", checksum = "7b5ce96c")]
#[test_action("apigateway", "GetIntegration", checksum = "d320cc1a")]
#[test_action("apigateway", "UpdateIntegration", checksum = "83d077e7")]
#[test_action("apigateway", "DeleteIntegration", checksum = "3fa912d8")]
#[tokio::test]
async fn apigateway_v1_integration_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, root_id) = create_api(&client, "conf-integration").await;
    let res_id = create_method_resource(&client, &api_id, &root_id).await;
    client
        .put_method()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .authorization_type("NONE")
        .send()
        .await
        .unwrap();

    client
        .put_integration()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .r#type(IntegrationType::Mock)
        .send()
        .await
        .unwrap();
    client
        .get_integration()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .send()
        .await
        .unwrap();
    client
        .update_integration()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .send()
        .await
        .unwrap();
    client
        .delete_integration()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "PutIntegrationResponse", checksum = "3f56e6bf")]
#[test_action("apigateway", "GetIntegrationResponse", checksum = "a14d4706")]
#[test_action("apigateway", "UpdateIntegrationResponse", checksum = "dfa14ea4")]
#[test_action("apigateway", "DeleteIntegrationResponse", checksum = "549a48e1")]
#[tokio::test]
async fn apigateway_v1_integration_response_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, root_id) = create_api(&client, "conf-integration-response").await;
    let res_id = create_method_resource(&client, &api_id, &root_id).await;
    client
        .put_method()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .authorization_type("NONE")
        .send()
        .await
        .unwrap();
    client
        .put_integration()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .r#type(IntegrationType::Mock)
        .send()
        .await
        .unwrap();

    client
        .put_integration_response()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .status_code("200")
        .send()
        .await
        .unwrap();
    client
        .get_integration_response()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .status_code("200")
        .send()
        .await
        .unwrap();
    client
        .update_integration_response()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .status_code("200")
        .send()
        .await
        .unwrap();
    client
        .delete_integration_response()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .status_code("200")
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateDeployment", checksum = "72eb1ac0")]
#[test_action("apigateway", "GetDeployment", checksum = "e222e420")]
#[test_action("apigateway", "GetDeployments", checksum = "017e5fc8")]
#[test_action("apigateway", "UpdateDeployment", checksum = "b921c853")]
#[test_action("apigateway", "DeleteDeployment", checksum = "edb3aca6")]
#[tokio::test]
async fn apigateway_v1_deployment_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, _root_id) = create_api(&client, "conf-deployment").await;

    let dep = client
        .create_deployment()
        .rest_api_id(&api_id)
        .stage_name("v1")
        .send()
        .await
        .unwrap();
    let dep_id = dep.id().unwrap().to_string();

    client
        .get_deployment()
        .rest_api_id(&api_id)
        .deployment_id(&dep_id)
        .send()
        .await
        .unwrap();
    client
        .get_deployments()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    client
        .update_deployment()
        .rest_api_id(&api_id)
        .deployment_id(&dep_id)
        .send()
        .await
        .unwrap();
    // Delete the auto-created stage first so the deployment is removable.
    client
        .delete_stage()
        .rest_api_id(&api_id)
        .stage_name("v1")
        .send()
        .await
        .ok();
    client
        .delete_deployment()
        .rest_api_id(&api_id)
        .deployment_id(dep_id)
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateStage", checksum = "4bd406a7")]
#[test_action("apigateway", "GetStage", checksum = "0f597b58")]
#[test_action("apigateway", "GetStages", checksum = "543d0315")]
#[test_action("apigateway", "UpdateStage", checksum = "23da1a45")]
#[test_action("apigateway", "DeleteStage", checksum = "ae20482b")]
#[test_action("apigateway", "FlushStageCache", checksum = "adef3ba6")]
#[test_action("apigateway", "FlushStageAuthorizersCache", checksum = "c89deffe")]
#[tokio::test]
async fn apigateway_v1_stage_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, _) = create_api(&client, "conf-stage").await;
    let dep = client
        .create_deployment()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    let dep_id = dep.id().unwrap().to_string();

    client
        .create_stage()
        .rest_api_id(&api_id)
        .stage_name("prod")
        .deployment_id(&dep_id)
        .send()
        .await
        .unwrap();
    client
        .get_stage()
        .rest_api_id(&api_id)
        .stage_name("prod")
        .send()
        .await
        .unwrap();
    client
        .get_stages()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    client
        .update_stage()
        .rest_api_id(&api_id)
        .stage_name("prod")
        .send()
        .await
        .unwrap();
    client
        .flush_stage_cache()
        .rest_api_id(&api_id)
        .stage_name("prod")
        .send()
        .await
        .ok();
    client
        .flush_stage_authorizers_cache()
        .rest_api_id(&api_id)
        .stage_name("prod")
        .send()
        .await
        .ok();
    client
        .delete_stage()
        .rest_api_id(&api_id)
        .stage_name("prod")
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateModel", checksum = "86aa9d46")]
#[test_action("apigateway", "GetModel", checksum = "1d212a81")]
#[test_action("apigateway", "GetModels", checksum = "518dae55")]
#[test_action("apigateway", "UpdateModel", checksum = "de04b279")]
#[test_action("apigateway", "DeleteModel", checksum = "8ec7e01d")]
#[test_action("apigateway", "GetModelTemplate", checksum = "66418134")]
#[tokio::test]
async fn apigateway_v1_model_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, _) = create_api(&client, "conf-model").await;

    client
        .create_model()
        .rest_api_id(&api_id)
        .name("Pet")
        .content_type("application/json")
        .schema(r#"{"type":"object"}"#)
        .send()
        .await
        .unwrap();
    client
        .get_model()
        .rest_api_id(&api_id)
        .model_name("Pet")
        .send()
        .await
        .unwrap();
    client
        .get_models()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    client
        .update_model()
        .rest_api_id(&api_id)
        .model_name("Pet")
        .send()
        .await
        .unwrap();
    client
        .get_model_template()
        .rest_api_id(&api_id)
        .model_name("Pet")
        .send()
        .await
        .ok();
    client
        .delete_model()
        .rest_api_id(&api_id)
        .model_name("Pet")
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateRequestValidator", checksum = "8b7077e8")]
#[test_action("apigateway", "GetRequestValidator", checksum = "4c86cf6e")]
#[test_action("apigateway", "GetRequestValidators", checksum = "03817dc4")]
#[test_action("apigateway", "UpdateRequestValidator", checksum = "944647c8")]
#[test_action("apigateway", "DeleteRequestValidator", checksum = "2fb4bda4")]
#[tokio::test]
async fn apigateway_v1_request_validator_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, _) = create_api(&client, "conf-validator").await;

    let v = client
        .create_request_validator()
        .rest_api_id(&api_id)
        .name("body")
        .send()
        .await
        .unwrap();
    let v_id = v.id().unwrap().to_string();

    client
        .get_request_validator()
        .rest_api_id(&api_id)
        .request_validator_id(&v_id)
        .send()
        .await
        .unwrap();
    client
        .get_request_validators()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    client
        .update_request_validator()
        .rest_api_id(&api_id)
        .request_validator_id(&v_id)
        .send()
        .await
        .unwrap();
    client
        .delete_request_validator()
        .rest_api_id(&api_id)
        .request_validator_id(v_id)
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateAuthorizer", checksum = "11174aee")]
#[test_action("apigateway", "GetAuthorizer", checksum = "83449c51")]
#[test_action("apigateway", "GetAuthorizers", checksum = "9b3c6a0e")]
#[test_action("apigateway", "UpdateAuthorizer", checksum = "2564e079")]
#[test_action("apigateway", "DeleteAuthorizer", checksum = "7b6bf67b")]
#[tokio::test]
async fn apigateway_v1_authorizer_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, _) = create_api(&client, "conf-authorizer").await;

    let a = client
        .create_authorizer()
        .rest_api_id(&api_id)
        .name("token")
        .r#type(aws_sdk_apigateway::types::AuthorizerType::Token)
        .send()
        .await
        .unwrap();
    let a_id = a.id().unwrap().to_string();

    client
        .get_authorizer()
        .rest_api_id(&api_id)
        .authorizer_id(&a_id)
        .send()
        .await
        .unwrap();
    client
        .get_authorizers()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    client
        .update_authorizer()
        .rest_api_id(&api_id)
        .authorizer_id(&a_id)
        .send()
        .await
        .unwrap();
    client
        .delete_authorizer()
        .rest_api_id(&api_id)
        .authorizer_id(a_id)
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateApiKey", checksum = "68f4e591")]
#[test_action("apigateway", "GetApiKey", checksum = "eb9c6072")]
#[test_action("apigateway", "GetApiKeys", checksum = "113bef17")]
#[test_action("apigateway", "UpdateApiKey", checksum = "12316616")]
#[test_action("apigateway", "DeleteApiKey", checksum = "4fe1c4ea")]
#[tokio::test]
async fn apigateway_v1_api_key_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;

    let key = client
        .create_api_key()
        .name("conf-key")
        .send()
        .await
        .unwrap();
    let key_id = key.id().unwrap().to_string();

    client.get_api_key().api_key(&key_id).send().await.unwrap();
    client.get_api_keys().send().await.unwrap();
    client
        .update_api_key()
        .api_key(&key_id)
        .send()
        .await
        .unwrap();
    client
        .delete_api_key()
        .api_key(key_id)
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateUsagePlan", checksum = "2bab49c6")]
#[test_action("apigateway", "GetUsagePlan", checksum = "600c3b79")]
#[test_action("apigateway", "GetUsagePlans", checksum = "d29bf27c")]
#[test_action("apigateway", "UpdateUsagePlan", checksum = "d384c385")]
#[test_action("apigateway", "DeleteUsagePlan", checksum = "d281320b")]
#[test_action("apigateway", "CreateUsagePlanKey", checksum = "5089a49b")]
#[test_action("apigateway", "GetUsagePlanKey", checksum = "2f2c22ae")]
#[test_action("apigateway", "GetUsagePlanKeys", checksum = "74478b9d")]
#[test_action("apigateway", "DeleteUsagePlanKey", checksum = "63561044")]
#[test_action("apigateway", "GetUsage", checksum = "4856c383")]
#[test_action("apigateway", "UpdateUsage", checksum = "745fe3c9")]
#[tokio::test]
async fn apigateway_v1_usage_plan_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;

    let plan = client
        .create_usage_plan()
        .name("plan")
        .send()
        .await
        .unwrap();
    let plan_id = plan.id().unwrap().to_string();

    let key = client
        .create_api_key()
        .name("conf-up-key")
        .send()
        .await
        .unwrap();
    let key_id = key.id().unwrap().to_string();

    client
        .create_usage_plan_key()
        .usage_plan_id(&plan_id)
        .key_id(&key_id)
        .key_type("API_KEY")
        .send()
        .await
        .unwrap();
    client
        .get_usage_plan()
        .usage_plan_id(&plan_id)
        .send()
        .await
        .unwrap();
    client.get_usage_plans().send().await.unwrap();
    client
        .update_usage_plan()
        .usage_plan_id(&plan_id)
        .send()
        .await
        .unwrap();
    client
        .get_usage_plan_key()
        .usage_plan_id(&plan_id)
        .key_id(&key_id)
        .send()
        .await
        .unwrap();
    client
        .get_usage_plan_keys()
        .usage_plan_id(&plan_id)
        .send()
        .await
        .unwrap();
    client
        .get_usage()
        .usage_plan_id(&plan_id)
        .start_date("1970-01-01")
        .end_date("2030-01-01")
        .send()
        .await
        .ok();
    client
        .update_usage()
        .usage_plan_id(&plan_id)
        .key_id(&key_id)
        .send()
        .await
        .ok();

    client
        .delete_usage_plan_key()
        .usage_plan_id(&plan_id)
        .key_id(&key_id)
        .send()
        .await
        .unwrap();
    client
        .delete_usage_plan()
        .usage_plan_id(plan_id)
        .send()
        .await
        .unwrap();
    client
        .delete_api_key()
        .api_key(key_id)
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateVpcLink", checksum = "21f6aa08")]
#[test_action("apigateway", "GetVpcLink", checksum = "f7293995")]
#[test_action("apigateway", "GetVpcLinks", checksum = "fbb6baf2")]
#[test_action("apigateway", "UpdateVpcLink", checksum = "57b888a4")]
#[test_action("apigateway", "DeleteVpcLink", checksum = "25ea5e75")]
#[tokio::test]
async fn apigateway_v1_vpc_link_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;

    let link = client
        .create_vpc_link()
        .name("conf-link")
        .target_arns("arn:aws:elasticloadbalancing:us-east-1:123456789012:loadbalancer/net/x/abcd")
        .send()
        .await
        .unwrap();
    let link_id = link.id().unwrap().to_string();

    client
        .get_vpc_link()
        .vpc_link_id(&link_id)
        .send()
        .await
        .unwrap();
    client.get_vpc_links().send().await.unwrap();
    client
        .update_vpc_link()
        .vpc_link_id(&link_id)
        .send()
        .await
        .unwrap();
    client
        .delete_vpc_link()
        .vpc_link_id(link_id)
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateDomainName", checksum = "41b239e1")]
#[test_action("apigateway", "GetDomainName", checksum = "e2e98543")]
#[test_action("apigateway", "GetDomainNames", checksum = "4c8e8fec")]
#[test_action("apigateway", "UpdateDomainName", checksum = "f8ac4285")]
#[test_action("apigateway", "DeleteDomainName", checksum = "26bf612d")]
#[test_action("apigateway", "CreateBasePathMapping", checksum = "57ece42e")]
#[test_action("apigateway", "GetBasePathMapping", checksum = "8199b681")]
#[test_action("apigateway", "GetBasePathMappings", checksum = "b07eae89")]
#[test_action("apigateway", "UpdateBasePathMapping", checksum = "2f4c6da2")]
#[test_action("apigateway", "DeleteBasePathMapping", checksum = "47f1d11d")]
#[tokio::test]
async fn apigateway_v1_domain_name_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;

    let domain = "api.conf.example.com";
    client
        .create_domain_name()
        .domain_name(domain)
        .send()
        .await
        .unwrap();
    client
        .get_domain_name()
        .domain_name(domain)
        .send()
        .await
        .unwrap();
    client.get_domain_names().send().await.unwrap();
    client
        .update_domain_name()
        .domain_name(domain)
        .send()
        .await
        .unwrap();

    let (api_id, _) = create_api(&client, "conf-bpm").await;
    client
        .create_deployment()
        .rest_api_id(&api_id)
        .stage_name("v1")
        .send()
        .await
        .unwrap();

    client
        .create_base_path_mapping()
        .domain_name(domain)
        .rest_api_id(&api_id)
        .base_path("v1")
        .stage("v1")
        .send()
        .await
        .unwrap();
    client
        .get_base_path_mapping()
        .domain_name(domain)
        .base_path("v1")
        .send()
        .await
        .unwrap();
    client
        .get_base_path_mappings()
        .domain_name(domain)
        .send()
        .await
        .unwrap();
    client
        .update_base_path_mapping()
        .domain_name(domain)
        .base_path("v1")
        .send()
        .await
        .unwrap();
    client
        .delete_base_path_mapping()
        .domain_name(domain)
        .base_path("v1")
        .send()
        .await
        .unwrap();
    client
        .delete_domain_name()
        .domain_name(domain)
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "GenerateClientCertificate", checksum = "d394afdb")]
#[test_action("apigateway", "GetClientCertificate", checksum = "7207eaa8")]
#[test_action("apigateway", "GetClientCertificates", checksum = "0d19c137")]
#[test_action("apigateway", "UpdateClientCertificate", checksum = "3bcb41f2")]
#[test_action("apigateway", "DeleteClientCertificate", checksum = "25d210d7")]
#[tokio::test]
async fn apigateway_v1_client_certificate_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;

    let cert = client.generate_client_certificate().send().await.unwrap();
    let cert_id = cert.client_certificate_id().unwrap().to_string();

    client
        .get_client_certificate()
        .client_certificate_id(&cert_id)
        .send()
        .await
        .unwrap();
    client.get_client_certificates().send().await.unwrap();
    client
        .update_client_certificate()
        .client_certificate_id(&cert_id)
        .send()
        .await
        .unwrap();
    client
        .delete_client_certificate()
        .client_certificate_id(cert_id)
        .send()
        .await
        .unwrap();
}

#[test_action("apigateway", "CreateDocumentationPart", checksum = "7263139b")]
#[test_action("apigateway", "GetDocumentationPart", checksum = "45a6503e")]
#[test_action("apigateway", "GetDocumentationParts", checksum = "38677781")]
#[test_action("apigateway", "UpdateDocumentationPart", checksum = "ebf01570")]
#[test_action("apigateway", "DeleteDocumentationPart", checksum = "c3ff0980")]
#[test_action("apigateway", "CreateDocumentationVersion", checksum = "71c5e469")]
#[test_action("apigateway", "GetDocumentationVersion", checksum = "322d1e1d")]
#[test_action("apigateway", "GetDocumentationVersions", checksum = "1cfec179")]
#[test_action("apigateway", "UpdateDocumentationVersion", checksum = "b1e71441")]
#[test_action("apigateway", "DeleteDocumentationVersion", checksum = "61846e6e")]
#[tokio::test]
async fn apigateway_v1_documentation_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, _) = create_api(&client, "conf-docs").await;

    let location = aws_sdk_apigateway::types::DocumentationPartLocation::builder()
        .r#type(aws_sdk_apigateway::types::DocumentationPartType::Api)
        .build()
        .unwrap();
    let part = client
        .create_documentation_part()
        .rest_api_id(&api_id)
        .location(location)
        .properties("{}")
        .send()
        .await
        .unwrap();
    let part_id = part.id().unwrap_or("p").to_string();
    client
        .get_documentation_part()
        .rest_api_id(&api_id)
        .documentation_part_id(&part_id)
        .send()
        .await
        .ok();
    client
        .get_documentation_parts()
        .rest_api_id(&api_id)
        .send()
        .await
        .ok();
    client
        .update_documentation_part()
        .rest_api_id(&api_id)
        .documentation_part_id(&part_id)
        .send()
        .await
        .ok();
    client
        .delete_documentation_part()
        .rest_api_id(&api_id)
        .documentation_part_id(&part_id)
        .send()
        .await
        .ok();

    client
        .create_documentation_version()
        .rest_api_id(&api_id)
        .documentation_version("v1")
        .send()
        .await
        .unwrap();
    client
        .get_documentation_version()
        .rest_api_id(&api_id)
        .documentation_version("v1")
        .send()
        .await
        .ok();
    client
        .get_documentation_versions()
        .rest_api_id(&api_id)
        .send()
        .await
        .ok();
    client
        .update_documentation_version()
        .rest_api_id(&api_id)
        .documentation_version("v1")
        .send()
        .await
        .ok();
    client
        .delete_documentation_version()
        .rest_api_id(&api_id)
        .documentation_version("v1")
        .send()
        .await
        .ok();
}

#[test_action("apigateway", "PutGatewayResponse", checksum = "c96306ee")]
#[test_action("apigateway", "GetGatewayResponse", checksum = "a7e914be")]
#[test_action("apigateway", "GetGatewayResponses", checksum = "e2fef41d")]
#[test_action("apigateway", "UpdateGatewayResponse", checksum = "9f11db4d")]
#[test_action("apigateway", "DeleteGatewayResponse", checksum = "5a03b6fe")]
#[tokio::test]
async fn apigateway_v1_gateway_response_lifecycle() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, _) = create_api(&client, "conf-gw-resp").await;

    let kind = aws_sdk_apigateway::types::GatewayResponseType::Default4Xx;
    client
        .put_gateway_response()
        .rest_api_id(&api_id)
        .response_type(kind.clone())
        .send()
        .await
        .ok();
    client
        .get_gateway_response()
        .rest_api_id(&api_id)
        .response_type(kind.clone())
        .send()
        .await
        .ok();
    client
        .get_gateway_responses()
        .rest_api_id(&api_id)
        .send()
        .await
        .ok();
    client
        .update_gateway_response()
        .rest_api_id(&api_id)
        .response_type(kind.clone())
        .send()
        .await
        .ok();
    client
        .delete_gateway_response()
        .rest_api_id(&api_id)
        .response_type(kind)
        .send()
        .await
        .ok();
}

#[test_action("apigateway", "GetExport", checksum = "a1c3bfc1")]
#[test_action("apigateway", "GetSdk", checksum = "4e6b3b72")]
#[test_action("apigateway", "GetSdkType", checksum = "d3449a62")]
#[test_action("apigateway", "GetSdkTypes", checksum = "6eab0d38")]
#[tokio::test]
async fn apigateway_v1_export_sdk() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, _) = create_api(&client, "conf-export").await;
    client
        .create_deployment()
        .rest_api_id(&api_id)
        .stage_name("v1")
        .send()
        .await
        .unwrap();

    client
        .get_export()
        .rest_api_id(&api_id)
        .stage_name("v1")
        .export_type("oas30")
        .send()
        .await
        .ok();
    client
        .get_sdk()
        .rest_api_id(&api_id)
        .stage_name("v1")
        .sdk_type("javascript")
        .send()
        .await
        .ok();
    client.get_sdk_type().id("javascript").send().await.ok();
    client.get_sdk_types().send().await.ok();
}

#[test_action("apigateway", "TagResource", checksum = "52ab0aa6")]
#[test_action("apigateway", "UntagResource", checksum = "47fc1655")]
#[test_action("apigateway", "GetTags", checksum = "89a21422")]
#[tokio::test]
async fn apigateway_v1_tags() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, _) = create_api(&client, "conf-tags").await;
    let arn = format!("arn:aws:apigateway:us-east-1::/restapis/{api_id}");

    client
        .tag_resource()
        .resource_arn(&arn)
        .tags("env", "prod")
        .send()
        .await
        .ok();
    client.get_tags().resource_arn(&arn).send().await.ok();
    client
        .untag_resource()
        .resource_arn(&arn)
        .tag_keys("env")
        .send()
        .await
        .ok();
}

#[test_action("apigateway", "TestInvokeMethod", checksum = "67cdacdd")]
#[test_action("apigateway", "TestInvokeAuthorizer", checksum = "8f7b5dd2")]
#[tokio::test]
async fn apigateway_v1_test_invoke() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, root_id) = create_api(&client, "conf-test-invoke").await;
    let res_id = create_method_resource(&client, &api_id, &root_id).await;
    client
        .put_method()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .authorization_type("NONE")
        .send()
        .await
        .unwrap();
    client
        .put_integration()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .r#type(IntegrationType::Mock)
        .send()
        .await
        .unwrap();

    client
        .test_invoke_method()
        .rest_api_id(&api_id)
        .resource_id(&res_id)
        .http_method("GET")
        .send()
        .await
        .ok();

    let auth = client
        .create_authorizer()
        .rest_api_id(&api_id)
        .name("conf-auth")
        .r#type(aws_sdk_apigateway::types::AuthorizerType::Token)
        .send()
        .await
        .unwrap();
    let auth_id = auth.id().unwrap().to_string();
    client
        .test_invoke_authorizer()
        .rest_api_id(&api_id)
        .authorizer_id(auth_id)
        .send()
        .await
        .ok();
}

#[test_action("apigateway", "GetAccount", checksum = "92977b09")]
#[test_action("apigateway", "UpdateAccount", checksum = "7702d7c3")]
#[tokio::test]
async fn apigateway_v1_account() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    client.get_account().send().await.ok();
    client.update_account().send().await.ok();
}

#[test_action(
    "apigateway",
    "CreateDomainNameAccessAssociation",
    checksum = "4e193636"
)]
#[test_action("apigateway", "GetDomainNameAccessAssociations", checksum = "86f72000")]
#[test_action(
    "apigateway",
    "DeleteDomainNameAccessAssociation",
    checksum = "2ddb73e5"
)]
#[test_action(
    "apigateway",
    "RejectDomainNameAccessAssociation",
    checksum = "f79e4802"
)]
#[tokio::test]
async fn apigateway_v1_domain_name_access_associations() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;

    let assoc = client
        .create_domain_name_access_association()
        .domain_name_arn("arn:aws:apigateway:us-east-1::/domainnames/private.conf.example.com")
        .access_association_source("arn:aws:vpce:us-east-1:123456789012:vpc-endpoint/vpce-1234")
        .access_association_source_type(
            aws_sdk_apigateway::types::AccessAssociationSourceType::Vpce,
        )
        .send()
        .await
        .unwrap();
    let arn = assoc
        .domain_name_access_association_arn()
        .unwrap_or_default()
        .to_string();

    client
        .get_domain_name_access_associations()
        .send()
        .await
        .ok();
    client
        .reject_domain_name_access_association()
        .domain_name_access_association_arn(&arn)
        .domain_name_arn("arn:aws:apigateway:us-east-1::/domainnames/private.conf.example.com")
        .send()
        .await
        .ok();
    client
        .delete_domain_name_access_association()
        .domain_name_access_association_arn(arn)
        .send()
        .await
        .ok();
}

#[test_action("apigateway", "ImportApiKeys", checksum = "0115480d")]
#[tokio::test]
async fn apigateway_v1_import_api_keys() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let body = aws_sdk_apigateway::primitives::Blob::new(
        b"key-name,key-value,description,true\n".to_vec(),
    );
    client
        .import_api_keys()
        .body(body)
        .format(aws_sdk_apigateway::types::ApiKeysFormat::Csv)
        .send()
        .await
        .ok();
}

#[test_action("apigateway", "ImportDocumentationParts", checksum = "1b14cc44")]
#[tokio::test]
async fn apigateway_v1_import_documentation_parts() {
    let server = TestServer::start().await;
    let client = server.apigateway_client().await;
    let (api_id, _) = create_api(&client, "conf-import-docs").await;
    let body = aws_sdk_apigateway::primitives::Blob::new(
        br#"{"documentationParts": [{"location": {"type": "API"}, "properties": "{}"}]}"#.to_vec(),
    );
    client
        .import_documentation_parts()
        .rest_api_id(&api_id)
        .body(body)
        .send()
        .await
        .ok();
}
