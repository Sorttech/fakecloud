mod helpers;

use aws_sdk_apigateway::types::{ConnectionType, IntegrationType};
use aws_sdk_elasticloadbalancingv2::types::{
    Action, ActionTypeEnum, LoadBalancerSchemeEnum, LoadBalancerTypeEnum, ProtocolEnum,
    TargetDescription, TargetTypeEnum,
};
use helpers::TestServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// An HTTP server that answers every request with its own request head, so a test can see
/// exactly what was sent to it: the request line and the headers.
async fn echo_backend() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut chunk = [0u8; 2048];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&chunk[..n]),
                    }
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    head.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.write_all(&head).await;
            });
        }
    });
    addr
}

/// The shape the onboarding stack deploys: a `{proxy+}` resource whose `HTTP_PROXY` integration
/// goes through a VPC link to a network load balancer's listener, mapping the greedy path onto the
/// backend URI. API Gateway has to resolve that listener to the target registered behind it; the
/// name in the integration URI is the load balancer's, which resolves nowhere.
#[tokio::test]
async fn a_proxy_resource_through_a_vpc_link_reaches_the_target_behind_the_nlb_listener() {
    let server = TestServer::start().await;
    let elbv2 = server.elbv2_client().await;
    let apigw = server.apigateway_client().await;
    let backend = echo_backend().await;

    let lb = elbv2
        .create_load_balancer()
        .name("vpclink-nlb")
        .r#type(LoadBalancerTypeEnum::Network)
        .scheme(LoadBalancerSchemeEnum::Internal)
        .send()
        .await
        .expect("create_load_balancer");
    let lb = lb
        .load_balancers()
        .first()
        .expect("a load balancer")
        .clone();
    let lb_arn = lb.load_balancer_arn().unwrap().to_string();
    let dns = lb.dns_name().unwrap().to_string();

    let tg = elbv2
        .create_target_group()
        .name("vpclink-tg")
        .protocol(ProtocolEnum::Tcp)
        .port(80)
        .target_type(TargetTypeEnum::Ip)
        .send()
        .await
        .expect("create_target_group");
    let tg_arn = tg
        .target_groups()
        .first()
        .unwrap()
        .target_group_arn()
        .unwrap()
        .to_string();
    elbv2
        .register_targets()
        .target_group_arn(&tg_arn)
        .targets(
            TargetDescription::builder()
                .id(backend.ip().to_string())
                .port(i32::from(backend.port()))
                .build(),
        )
        .send()
        .await
        .expect("register_targets");
    elbv2
        .create_listener()
        .load_balancer_arn(&lb_arn)
        .protocol(ProtocolEnum::Tcp)
        .port(8001)
        .default_actions(
            Action::builder()
                .r#type(ActionTypeEnum::Forward)
                .target_group_arn(&tg_arn)
                .build(),
        )
        .send()
        .await
        .expect("create_listener");

    let link = apigw
        .create_vpc_link()
        .name("link")
        .target_arns(&lb_arn)
        .send()
        .await
        .expect("create_vpc_link");
    let link_id = link.id().unwrap().to_string();

    let api = apigw
        .create_rest_api()
        .name("proxied")
        .send()
        .await
        .expect("create_rest_api");
    let api_id = api.id().unwrap().to_string();
    let root = apigw
        .get_resources()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap()
        .items()
        .iter()
        .find(|r| r.path() == Some("/"))
        .and_then(|r| r.id())
        .unwrap()
        .to_string();
    let proxy = apigw
        .create_resource()
        .rest_api_id(&api_id)
        .parent_id(&root)
        .path_part("{proxy+}")
        .send()
        .await
        .expect("create_resource");
    let resource = proxy.id().unwrap().to_string();
    apigw
        .put_method()
        .rest_api_id(&api_id)
        .resource_id(&resource)
        .http_method("ANY")
        .authorization_type("NONE")
        .request_parameters("method.request.path.proxy", true)
        .send()
        .await
        .expect("put_method");
    apigw
        .put_integration()
        .rest_api_id(&api_id)
        .resource_id(&resource)
        .http_method("ANY")
        .r#type(IntegrationType::HttpProxy)
        .integration_http_method("ANY")
        .uri(format!("http://{dns}:8001/{{proxy}}"))
        .connection_type(ConnectionType::VpcLink)
        .connection_id(&link_id)
        .request_parameters(
            "integration.request.path.proxy",
            "method.request.path.proxy",
        )
        .request_parameters("integration.request.header.x-mapped", "'yes'")
        .send()
        .await
        .expect("put_integration");
    apigw
        .create_deployment()
        .rest_api_id(&api_id)
        .stage_name("prod")
        .send()
        .await
        .expect("create_deployment");

    let url = format!(
        "{}/restapis/{api_id}/prod/_user_request_/api/ping?page=2",
        server.endpoint()
    );
    let response = reqwest::Client::new().get(&url).send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap().to_lowercase();
    assert_eq!(status, 200, "{body}");
    // The greedy path went into the backend URI, the client's query string was passed through,
    // and the mapped header was added, all on the request that reached the registered target.
    assert!(body.starts_with("get /api/ping?page=2 http/1.1"), "{body}");
    assert!(body.contains("x-mapped: yes"), "{body}");
}
