use std::collections::HashMap;
use std::thread;
use std::ops::Deref;
use std::sync::{Arc, Mutex, mpsc, RwLock};

use chrono::prelude::*;
use redis::{self, Commands};
use reqwest;
use serde_json;
use ws;
use ws::util::Token;
use ws::{Error, Handler, Handshake, Message, Sender};

use exchange::{self, Asset, AssetExchange, Exchange};
use orderbook;

const EXPIRE: Token = Token(1);

/// Exchange related metadata. The fields are used to establish
/// a successful connection with the exchange via websockets.
#[derive(Clone)]
pub struct WSExchange {
    /// Full URL to connect to. Example: `wss://www.bitmex.com/realtime`
    pub host: String,

    /// Indicate whether or not we've received the snapshot message yet
    pub snapshot_received: bool,

    /// Collection metadata
    pub metadata: MetaData,

    /// Channel name with no argument we want to subscribe to
    pub single_channels: Vec<String>,
    /// Channel name as map key/value pair
    pub dual_channels: Vec<String>,

    /// BitMEX requires asset indexes to calculate asset price
    pub asset_indexes: HashMap<String, u64>,
    /// Allows us to calculate the price of a given asset in combination with [`asset_indexes`]
    pub asset_tick_size: HashMap<String, f32>,

    /// TectonicDB connection
    pub tectonic: orderbook::tectonic::TectonicConnection,

    /// Redis client (before connection)
    pub r: redis::Client,
    /// Redis password: If this is present, we will send an AUTH message to the server on connect
    pub r_password: Option<String>,

    /// Thread channel. We will use this to communicate with a secondary connection
    /// opened after a 15 minute count to ensure a stable connection. This channel is
    /// managed by SocketManager
    pub channel: Option<mpsc::Sender<orderbook::Delta>>,
}

/// Create two identical structs and transfer the data over when we start the websocket.
pub struct WSExchangeSender {
    /// Full URL to connect to. Example: `wss://www.bitmex.com/realtime`
    host: String,

    /// Indicate whether or not we've received the snapshot message yet
    snapshot_received: bool,

    /// Optional function that can be called as a callback per message received.
    /// Usually, this will send a delta, but we will make it generic to allow for flexability
    //callback: Option<Box<Fn(&orderbook::Delta)>>,

    /// Collection metadata
    metadata: MetaData,

    /// Channel name with no argument we want to subscribe to
    single_channels: Vec<String>,
    /// Channel name as map key/value pair
    dual_channels: Vec<String>,

    /// BitMEX requires asset indexes to calculate asset price
    asset_indexes: Arc<RwLock<HashMap<String, u64>>>,
    /// Allows us to calculate the price of a given asset in combination with [`asset_indexes`]
    asset_tick_size: Arc<RwLock<HashMap<String, f32>>>,

    /// TectonicDB connection
    tectonic: orderbook::tectonic::TectonicConnection,
    /// Redis client (used to send deltas as PUBSUB)
    r: Arc<Mutex<redis::Connection>>,

    /// Websocket sender
    out: Sender,
}

/// Meta data for our data source. This is useful for data warehousing and accessing the data.
/// All types contained within are considered optional. This may be expanded in the future.
#[derive(Clone)]
pub struct MetaData {
    /// Vector of asset pairs we're going to warehouse
    pub asset_pair: Option<Vec<[exchange::Asset; 2]>>,

    /// Starting datetime of our data collection
    start_date: Option<DateTime<Utc>>,

    /// Ending datetime of our data collection
    end_date: Option<DateTime<Utc>>,
}

/// Master bitmex message. This may contain a delta or a snapshot
#[derive(Serialize, Deserialize, Debug)]
struct BitMEXMessage {
    /// Specifies where update originates from (i.e. channel)
    table: String,
    /// Tells if action is a snapshot or delta
    action: String,
    /// Snapshot or delta data
    data: Vec<BitMEXData>,
}

/// BitMEX websocket data. All deltas and snapshot updates are sent as such
#[derive(Serialize, Deserialize, Debug)]
struct BitMEXData {
    /// Asset-pair name
    symbol: String,
    /// Orderbook side (bid/ask)
    side: String,
    /// Price comes encoded in this value.
    id: Option<u64>,
    /// Order size. If not present, then it is a level removal
    size: Option<f32>,
    /// Only present on insert and snapshot events
    price: Option<f32>
}

#[derive(Serialize, Deserialize)]
struct AssetInformation {
    symbol: String,
    timestamp: String,

    #[serde(rename = "tickSize")]
    tick_size: f32,
}

impl AssetExchange for WSExchange {
    fn default_settings() -> Result<Box<Self>, String> {
        let settings = Self {
            host: "wss://www.bitmex.com/realtime".into(),

            snapshot_received: false,

            //callback: None,

            metadata: MetaData {
                asset_pair: Some(vec![
                    [Asset::BTC, Asset::USD],]),
                start_date: None,
                end_date: None,
            },

            single_channels: vec![],
            dual_channels: vec!["orderBookL2".into(), "trade".into()],

            asset_indexes: HashMap::new(),
            asset_tick_size: HashMap::new(),

            tectonic: orderbook::tectonic::TectonicConnection::new(None, None).expect("Unable to connect to TectonicDB"),
            r: redis::Client::open("redis://localhost").unwrap(),
            r_password: None,

            channel: None,
        };

        Ok(Box::new(settings))
    }

    fn init_redis(&mut self) -> Result<redis::Connection, redis::RedisError> {
        let redis_connection = self.r.clone()
            .get_connection()
            .unwrap();

        // Send an auth message if we have a password
        match &self.r_password {
            Some(password) => {
                redis::cmd("AUTH").arg(password)
                    .execute(&redis_connection);
            },
            None => (),
        };

        Ok(redis_connection)
    }

    fn run(settings: Option<&Self>) {
        // Try to use the settings the user passes before resorting to default settings.
        let mut settings = settings.cloned().unwrap_or(*WSExchange::default_settings().unwrap());

        ws::connect(settings.host.clone(), |out| WSExchangeSender {
            host: settings.host.clone(),

            snapshot_received: settings.snapshot_received.clone(),
            metadata: settings.metadata.clone(),

            single_channels: settings.single_channels.clone(),
            dual_channels: settings.dual_channels.clone(),
            
            asset_indexes: Arc::new(RwLock::new(settings.asset_indexes.clone())),
            asset_tick_size: Arc::new(RwLock::new(settings.asset_tick_size.clone())),

            tectonic: settings.tectonic.clone(),
            r: Arc::new(Mutex::new(settings.init_redis().expect("Failed to connect to Redis server."))),

            out,
        }).unwrap();
    }
}

#[derive(Serialize, Deserialize)]
struct BitMEXSubscription {
    op: String,
    args: Vec<String>,
}

impl Handler for WSExchangeSender {
    fn on_open(&mut self, _: Handshake) -> Result<(), Error> {
        // Set a timeout for 5 seconds of inactivity
        // Issue: currently, this reruns every five seconds. Disable for the meanwhile 
        // while we fix this issue
        // self.out.timeout(5_000, EXPIRE).unwrap();

        let mut msg = BitMEXSubscription {
            op: "subscribe".into(),
            args: vec![],
        };

        for channel in &self.single_channels {
            msg.args.push(channel.to_string());
        }

        for key in &self.dual_channels {
            for pair in self.metadata.asset_pair.as_ref().expect("No assets supplied to BitMEX struct") {
                msg.args.push(format!("{}:{}", key, exchange::get_asset_pair(pair, Exchange::BitMEX)));
            }
        }

        println!("{}", serde_json::to_string(&msg).unwrap());

        // Now that we've built our message, let's get the indicies of the assets we can trade
        let response: Vec<AssetInformation> = reqwest::get("https://www.bitmex.com/api/v1/instrument?columns=symbol,tickSize&start=0&count=500")
            .expect("Failed to send request")
            .json()
            .expect("Failed to serialize response to JSON");

        for (index, asset) in response.iter().enumerate() {
            // Dereference Arc and mutate after locking the RwLock
            self.asset_indexes.deref()
                .write()
                .unwrap()
                .insert(asset.symbol.clone(), index as u64);

            self.asset_tick_size.deref()
                .write()
                .unwrap()
                .insert(asset.symbol.clone(), asset.tick_size);

            if !self.tectonic.exists(format!("bitmex_{}", asset.symbol.clone()))? && 
                asset.symbol.clone() == exchange::get_asset_pair(
                    &[exchange::Asset::BTC, exchange::Asset::USD], 
                    exchange::Exchange::BitMEX)
                {

                // Create tectonic database if it doesn't exist yet. This avoids many issues
                // relating to inserting to a non-existant database.
                let _ = self.tectonic.create(format!("bitmex_{}", asset.symbol.clone()));
            }
        }

        // Send our constructed message to the server
        self.out.send(serde_json::to_string(&msg).unwrap())
    }

    fn on_message(&mut self, msg: Message) -> Result<(), Error> {
        let redis_ref = self.r.clone();
        let asset_tick_ref = self.asset_tick_size.clone();
        let asset_index_ref = self.asset_indexes.clone();

        // Spawn thread to ensure accurate timestamps
        thread::spawn(move || {
            match serde_json::from_slice::<BitMEXMessage>(&msg.into_data()) {
                Ok(message) => {
                    // Skip snapshots and other misc. data
                    if message.table == "" || message.table == "partial" {
                        return;
                    }
                    // Define a timestamp for the messages received
                    let ts = Utc::now().timestamp_millis() as f64 * 0.001f64;
                    let mut deltas: Vec<orderbook::Delta> = Vec::with_capacity(message.data.len());

                    for update in message.data {
                        // Let's make sure we don't parse any values with no ID
                        if update.id.is_none() {
                            continue;
                        }

                        let is_bid = match update.side == "Buy" {
                            true => orderbook::BID,
                            false => orderbook::ASK,
                        };
                        let is_trade = match message.action == "Trade" {
                            true => orderbook::TRADE,
                            false => orderbook::UPDATE,
                        };
                    
                        let delta = if update.symbol == "XBTUSD" {
                            orderbook::Delta {
                                symbol: String::from("XBTUSD"),
                                price: (8800000000 - update.id.unwrap()) as f32 * 0.01,
                                size: update.size.unwrap_or(0.0),
                                seq: 0,
                                event: is_bid ^ is_trade,
                                ts,
                            }
                        } else {
                            // Avoids borrowing [`update.symbol`] by changing the order the elements are assigned
                            orderbook::Delta {
                                price: ((100000000 * asset_index_ref.as_ref()
                                    .read()
                                    .unwrap()[&update.symbol]) - update.id.unwrap()
                                ) as f32 * asset_tick_ref.as_ref()
                                    .read()
                                    .unwrap()[&update.symbol],

                                symbol: update.symbol,
                                size: update.size.unwrap_or(0.0),
                                seq: 0,
                                event: is_bid ^ is_trade,
                                ts,
                            }
                        };

                        deltas.push(delta);
                    }

                    // Lock the connection until we are able to aquire it
                    let _ = redis_ref.as_ref()
                        .lock()
                        .unwrap()
                        .publish::<&str, &str, u8>("bitmex", &serde_json::to_string(&deltas).unwrap())
                        .expect("Failed to publish message to redis PUBSUB");
                },

                Err(e) => {
                    println!("Error encountered: {}", e);
                    return;
                },
            }
        });

        Ok(())
    }

    fn on_close(&mut self, _: ws::CloseCode, _: &str) {
        // TODO: Have proper handling of disconnect events. We should be handling disconnects more gracefully
        // instead of just reconnecting. We need to be prepared for them and handle data accordingly.
        println!("BitMEX Socket is closing. Opening a new connection...");

        ws::connect(self.host.clone(), |out| WSExchangeSender{
            host: self.host.clone(),
            snapshot_received: false,
            metadata: self.metadata.clone(),

            single_channels: self.single_channels.clone(),
            dual_channels: self.dual_channels.clone(),

            asset_indexes: self.asset_indexes.clone(),
            asset_tick_size: self.asset_tick_size.clone(),

            tectonic: self.tectonic.clone(),
            r: self.r.clone(),

            out,
        }).unwrap();
    }

    fn on_timeout(&mut self, _: Token) -> Result<(), ws::Error> {
        // TODO: Have proper handling of disconnect events. We should be handling disconnects more gracefully
        // instead of just reconnecting. We need to be prepared for them and handle data accordingly.
        println!("BitMEX Socket timed out (5s of inactivity). Opening a new connection...");

        ws::connect(self.host.clone(), |out| WSExchangeSender{
            host: self.host.clone(),
            snapshot_received: false,
            metadata: self.metadata.clone(),

            single_channels: self.single_channels.clone(),
            dual_channels: self.dual_channels.clone(),

            asset_indexes: self.asset_indexes.clone(),
            asset_tick_size: self.asset_tick_size.clone(),

            tectonic: self.tectonic.clone(),
            r: self.r.clone(),

            out,
        }).unwrap();

        Ok(())
    }
}