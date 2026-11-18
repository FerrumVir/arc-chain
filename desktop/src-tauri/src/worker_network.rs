//! Quiet, bounded scoreboard reads. No worker identities cross IPC.
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct WorkerNetwork {
    pub count_total: u64,
    pub count_visible: Option<u64>,
    pub eligible_inference_workers: Option<u64>,
}

#[derive(Default)]
pub struct Cache(Option<(String, Instant, Option<WorkerNetwork>)>);

impl Cache {
    async fn read(&mut self, http: &reqwest::Client, host: &str) -> Option<WorkerNetwork> {
        if let Some((origin, at, value)) = &self.0 {
            if origin == host && at.elapsed() < Duration::from_secs(60) {
                return value.clone();
            }
        }
        let value = async {
            let response = http
                .get(format!("{}/workers/scoreboard", host.trim_end_matches('/')))
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            response.json::<WorkerNetwork>().await.ok()
        }
        .await;
        // Cache absence too: a missing endpoint must not cause a request storm.
        self.0 = Some((host.to_owned(), Instant::now(), value.clone()));
        value
    }
}

pub async fn fetch(http: &reqwest::Client, host: &str) -> Option<WorkerNetwork> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE
        .get_or_init(|| Mutex::new(Cache::default()))
        .lock()
        .await
        .read(http, host)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn server(responses: Vec<String>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0; 2048];
                let n = socket.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]);
                assert!(request.starts_with("GET /workers/scoreboard HTTP/1.1"));
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (host, task)
    }

    fn response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn worker_network_caches_success_and_absence_then_refreshes() {
        let body = r#"{"count_total":70,"count_visible":50,"eligible_inference_workers":4,"workers":[{"private":"ignored"}]}"#;
        let (host, server) = server(vec![
            response("200 OK", body),
            response("404 Not Found", ""),
        ])
        .await;
        let mut cache = Cache::default();
        let http = client();
        let first = cache.read(&http, &host).await.unwrap();
        assert_eq!(first.count_total, 70);
        assert_eq!(first.eligible_inference_workers, Some(4));
        assert!(!serde_json::to_string(&first).unwrap().contains("private"));
        assert_eq!(cache.read(&http, &host).await, Some(first));
        cache.0.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(61);
        assert_eq!(cache.read(&http, &host).await, None);
        assert_eq!(cache.read(&http, &host).await, None);
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn worker_network_missing_count_and_redirect_fail_quietly() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect = format!("HTTP/1.1 302 Found\r\nLocation: http://{}/workers/scoreboard\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", target.local_addr().unwrap());
        let (host, server) =
            server(vec![response("200 OK", r#"{"count_visible":4}"#), redirect]).await;
        let http = client();
        let mut cache = Cache::default();
        assert_eq!(cache.read(&http, &host).await, None);
        cache.0.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(61);
        assert_eq!(cache.read(&http, &host).await, None);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), target.accept())
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn worker_network_cache_does_not_cross_origins() {
        let (one, first) = server(vec![response("200 OK", r#"{"count_total":1}"#)]).await;
        let (two, second) = server(vec![response("200 OK", r#"{"count_total":2}"#)]).await;
        let mut cache = Cache::default();
        assert_eq!(cache.read(&client(), &one).await.unwrap().count_total, 1);
        assert_eq!(cache.read(&client(), &two).await.unwrap().count_total, 2);
        first.await.unwrap();
        second.await.unwrap();
    }
}
