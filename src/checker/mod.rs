use anyhow::anyhow;
use reqwest::Client;
use serde::Deserialize;
use slog::{error, info, o, Drain, Logger};
use std::time::Duration;
use tokio::runtime::Runtime;

pub fn main(host: String) -> anyhow::Result<()> {
    let logger = slog::Logger::root(slog_journald::JournaldDrain.ignore_res(), o!());

    let rt = Runtime::new()?;
    info!(logger, "Started Tokio runtime");
    rt.block_on(async_main(&logger.new(o!("host" => host.clone())), &host))
}

async fn async_main(logger: &Logger, host: &str) -> anyhow::Result<()> {
    info!(logger, "Started the checker");

    let client = reqwest::ClientBuilder::new()
        // TODO: set a User Agent with a URL that describes the bot
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| {
            let msg = format!("Failed to prepare a reqwest client: {}", err);
            error!(logger, "{}", &msg);
            anyhow!(msg)
        })?;

    let software = get_software(logger, &client, host).await?;
    info!(logger, "{} runs {}", host, software);

    Ok(())
}

async fn get_software(logger: &Logger, client: &Client, host: &str) -> anyhow::Result<String> {
    let nodeinfo = fetch_nodeinfo(logger, client, host).await?;
    json::parse(&nodeinfo)
        .map(|obj| obj["software"]["name"].to_string())
        .map_err(|err| {
            let msg = format!(
                "Failed to figure out the software name from the NodeInfo {}: {}",
                nodeinfo, err
            );
            error!(logger, "{}", &msg; "json_error" => err.to_string());
            anyhow!(msg)
        })
}

#[derive(Debug, Deserialize)]
struct NodeInfoPointer {
    links: Vec<NodeInfoPointerLink>,
}

#[derive(Debug, Deserialize)]
struct NodeInfoPointerLink {
    rel: String,
    href: String,
}

async fn fetch_nodeinfo(logger: &Logger, client: &Client, host: &str) -> anyhow::Result<String> {
    let pointer = fetch_nodeinfo_pointer(logger, client, host).await?;
    // TODO: add sanitization step that removes any links that point outside of the current host's
    // domain
    let url = pick_highest_supported_nodeinfo_version(&pointer).ok_or_else(|| {
        let msg = format!(
            "Failed to pick the highest supported NodeInfo version out of {:?}",
            pointer.links
        );
        error!(logger, "{}", &msg);
        anyhow!(msg)
    })?;
    fetch_nodeinfo_document(logger, client, &url).await
}

async fn fetch_nodeinfo_pointer(
    logger: &Logger,
    client: &Client,
    host: &str,
) -> anyhow::Result<NodeInfoPointer> {
    let url = format!("https://{}/.well-known/nodeinfo", host);
    let response = client
        .get(&url)
        .header(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        )
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    response.error_for_status_ref().map_err(|err| {
        error!(
            logger, "Failed to fetch the well-known NodeInfo document: {}", err;
            "http_error" => err.to_string(), "url" => url);
        err
    })?;

    // TODO: replace this with a parser that only processes the first few KB of the input
    Ok(response.json::<NodeInfoPointer>().await?)
}

fn pick_highest_supported_nodeinfo_version(pointer: &NodeInfoPointer) -> Option<String> {
    // This array in the ascending order of schema versions.
    const SUPPORTED_NODEINFO_SCHEMAS: [&'static str; 4] = [
        "http://nodeinfo.diaspora.software/ns/schema/1.0",
        "http://nodeinfo.diaspora.software/ns/schema/1.1",
        "http://nodeinfo.diaspora.software/ns/schema/2.0",
        "http://nodeinfo.diaspora.software/ns/schema/2.1",
    ];
    pointer
        .links
        .iter()
        .filter_map(|link| {
            SUPPORTED_NODEINFO_SCHEMAS
                .iter()
                .position(|el| el == &link.rel)
                .map(|priority| (priority, link))
        })
        .max_by_key(|result| result.0)
        .map(|result| result.1.href.clone())
}

async fn fetch_nodeinfo_document(
    logger: &Logger,
    client: &Client,
    url: &str,
) -> anyhow::Result<String> {
    let response = client
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        )
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    response.error_for_status_ref().map_err(|err| {
        error!(
            logger, "Failed to fetch NodeInfo: {}", err;
            "http_error" => err.to_string(), "url" => url);
        err
    })?;

    // TODO: replace this with a parser that only processes the first few KB of the input
    Ok(response.text().await?)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn picks_highest_nodeinfo_version() {
        assert_eq!(
            pick_highest_supported_nodeinfo_version(&NodeInfoPointer { links: vec![] }),
            None,
        );

        assert_eq!(
            pick_highest_supported_nodeinfo_version(&NodeInfoPointer {
                links: vec![NodeInfoPointerLink {
                    rel: "http://nodeinfo.diaspora.software/ns/schema/2.2".to_string(),
                    href: "first".to_string()
                }],
            }),
            None,
        );

        assert_eq!(
            pick_highest_supported_nodeinfo_version(&NodeInfoPointer {
                links: vec![NodeInfoPointerLink {
                    rel: "http://nodeinfo.diaspora.software/ns/schema/1.0".to_string(),
                    href: "first".to_string()
                }],
            }),
            Some("first".to_string())
        );

        assert_eq!(
            pick_highest_supported_nodeinfo_version(&NodeInfoPointer {
                links: vec![
                    NodeInfoPointerLink {
                        rel: "http://nodeinfo.diaspora.software/ns/schema/1.0".to_string(),
                        href: "first".to_string()
                    },
                    NodeInfoPointerLink {
                        rel: "http://nodeinfo.diaspora.software/ns/schema/2.1".to_string(),
                        href: "2.1".to_string()
                    }
                ],
            }),
            Some("2.1".to_string())
        );

        assert_eq!(
            pick_highest_supported_nodeinfo_version(&NodeInfoPointer {
                links: vec![
                    NodeInfoPointerLink {
                        rel: "http://nodeinfo.diaspora.software/ns/schema/2.0".to_string(),
                        href: "highest is the first".to_string()
                    },
                    NodeInfoPointerLink {
                        rel: "http://nodeinfo.diaspora.software/ns/schema/1.1".to_string(),
                        href: "lowest is the second".to_string()
                    }
                ],
            }),
            Some("highest is the first".to_string())
        );
    }
}
