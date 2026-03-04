use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub helius_api_key: String,
    pub helius_http_url: String,
    pub helius_ws_url: String,
    pub port: u16,
    pub cache_capacity: u64,
}

impl Config {
    pub fn from_env() -> Self {
        let key = env::var("HELIUS_API_KEY").expect("HELIUS_API_KEY must be set");
        let network = env::var("NETWORK").unwrap_or_else(|_| "mainnet".into());
        let port: u16 = env::var("PORT")
            .unwrap_or_else(|_| "8899".into())
            .parse()
            .expect("PORT must be a valid u16");
        let cache_capacity: u64 = env::var("CACHE_SIZE")
            .unwrap_or_else(|_| "500000".into())
            .parse()
            .expect("CACHE_SIZE must be a valid u64");

        let http_host = format!("{}.helius-rpc.com", network);
        let helius_http_url = format!("https://{}/?api-key={}", http_host, key);
        let helius_ws_url = format!("wss://{}/?api-key={}", http_host, key);

        Config {
            helius_api_key: key,
            helius_http_url,
            helius_ws_url,
            port,
            cache_capacity,
        }
    }
}
