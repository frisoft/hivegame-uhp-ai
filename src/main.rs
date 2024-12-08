use std::process::{Command, Stdio, Child};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Semaphore};

mod turn_tracker;
use turn_tracker::{TurnTracker, TurnTracking};
mod ai;
mod hivegame_bot_api;
use hivegame_bot_api::HiveGameApi;

const MAX_CONCURRENT_PROCESSES: usize = 5;
const QUEUE_CAPACITY: usize = 1000;
const BASE_URL: &str = "http://your-server.com";

#[derive(Clone)]
struct Bot {
    name: String,
    uri: String,
    api_key: String,
    ai_command: String,
    bestmove_command_args: String,
}

struct GameTurn {
    game_string: String,
    hash: u64,
    bot: Bot,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bots = vec![
        Bot {
            name: "nokamute1".to_string(),
            uri: "/games/nokamute1".to_string(),
            api_key: "nokamute1_key".to_string(),
            ai_command: "../nokamute/target/debug/nokamute uhp --threads=1".to_string(),
            bestmove_command_args: "depth 1".to_string(),
        },
        Bot {
            name: "nokamute1".to_string(),
            uri: "/games/nokamute2".to_string(),
            api_key: "nokamute2_key".to_string(),
            ai_command: "../nokamute/target/debug/nokamute uhp".to_string(),
            bestmove_command_args: "time 00:00:01".to_string(),
        },
    ];

    let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
    let receiver = Arc::new(Mutex::new(receiver));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_PROCESSES));
    let active_processes = Arc::new(Mutex::new(Vec::new()));
    let turn_tracker = TurnTracker::new();
    
    let cleanup_tracker = turn_tracker.clone();
    tokio::spawn(async move {
        loop {
            // Clean up every 2 sec (Will increase later)
            tokio::time::sleep(Duration::from_secs(2)).await;
            cleanup_tracker.cleanup().await;
        }
    });
    
    // Spawn a producer task for each bot
    let mut producer_handles = Vec::new();
    for bot in bots {
        let producer_handle = tokio::spawn(producer_task(
            sender.clone(),
            turn_tracker.clone(),
            bot,
        ));
        producer_handles.push(producer_handle);
    }
    
    let consumer_handle = tokio::spawn(consumer_task(
        receiver,
        semaphore,
        active_processes,
        turn_tracker.clone(),
    ));

    // Wait for all producers and the consumer
    for handle in producer_handles {
        if let Err(e) = handle.await? {
            eprintln!("Producer error: {}", e);
        }
    }
    if let Err(e) = consumer_handle.await? {
        eprintln!("Consumer error: {}", e);
    }
    
    Ok(())
}

fn calculate_hash(game_string: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    game_string.hash(&mut hasher);
    hasher.finish()
}

async fn producer_task(
    sender: mpsc::Sender<GameTurn>,
    turn_tracker: TurnTracker,
    bot: Bot,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let api = HiveGameApi::new(BASE_URL.to_string());
    
    loop {
        match api.fake_get_games(&bot.uri, &bot.api_key).await {
            Ok(game_strings) => {
                for game_string in game_strings {
                    let hash = calculate_hash(&game_string);
                    
                    if turn_tracker.tracked(hash).await {
                        continue;
                    }

                    let turn = GameTurn {
                        game_string,
                        hash,
                        bot: bot.clone(),
                    };

                    turn_tracker.processing(hash).await;

                    if sender.send(turn).await.is_err() {
                        eprintln!("Failed to send turn to queue");
                        continue;
                    }
                }
            }
            Err(e) => eprintln!("Failed to fetch games for bot {}: {}", bot.name, e),
        }

        println!("Start new cycle in 1 sec");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn consumer_task(
    receiver: Arc<Mutex<mpsc::Receiver<GameTurn>>>,
    semaphore: Arc<Semaphore>,
    active_processes: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    turn_tracker: TurnTracker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let mut rx = receiver.lock().await;
        if let Some(turn) = rx.recv().await {
            drop(rx);
            
            let handle = tokio::spawn(process_turn(
                turn,
                semaphore.clone(),
                turn_tracker.clone(),
            ));

            active_processes.lock().await.push(handle);
            cleanup_processes(active_processes.clone()).await;
        }
    }
}

async fn process_turn(
    turn: GameTurn,
    semaphore: Arc<Semaphore>,
    turn_tracker: TurnTracker,
) {
    let _permit = semaphore.acquire().await.expect("Failed to acquire semaphore");

    let child = match ai::spawn_process(&turn.bot.ai_command, &turn.bot.name) {
        Ok(child) => child,
        Err(e) => {
            eprintln!("Failed to spawn AI process for bot {}: {}", turn.bot.name, e);
            turn_tracker.processed(turn.hash).await;
            return;
        }
    };

    match ai::run_commands(child, &turn.game_string, &turn.bot.bestmove_command_args).await {
        Ok(bestmove) => {
            println!("Bot '{}' bestmove: '{}'", turn.bot.name, bestmove);
            // Here you can handle the bestmove (e.g., send it to the server)
        }
        Err(e) => {
            eprintln!("Error running AI commands for bot '{}': '{}'", turn.bot.name, e);
        }
    }

    turn_tracker.processed(turn.hash).await;
}

async fn cleanup_processes(active_processes: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>) {
    let mut processes = active_processes.lock().await;
    processes.retain(|handle| !handle.is_finished());
}
