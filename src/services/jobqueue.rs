// use sqlx::{Postgres, Pool};
// use tokio::{time, sync::RwLock};
// use tracing::{debug, error, warn};

// use crate::{db, models::{AppState, Error}};
// use std::{sync::Arc, collections::HashMap};

// #[derive(Debug)]
// struct JobQueue {
//     pool: Pool<Postgres>,
//     running_jobs: Arc<RwLock<HashMap<i32, tokio::task::JoinHandle<()>>>>,
// }

// impl JobQueue {
//     async fn new(pool: Pool<Postgres>) -> Result<Self, sqlx::Error> {
//         let running_jobs = Arc::new(RwLock::new(HashMap::new()));
//         Ok(Self { pool, running_jobs })
//     }

//     async fn add_job(&self, payload: String, scheduled_at: chrono::NaiveDateTime) -> Result<(), sqlx::Error> {
//         sqlx::query(
//             "INSERT INTO jobs (payload, scheduled_at) VALUES ($1, $2)",
//         )
//         .bind(&payload)
//         .bind(&scheduled_at)
//         .execute(&self.pool)
//         .await?;
//         Ok(())
//     }

//     async fn schedule_jobs(&self) {
//         let mut interval = interval(Duration::from_secs(1));
//         loop {
//             interval.tick().await;
//             self.run_scheduled_jobs().await;
//         }
//     }

    
//     async fn run_scheduled_jobs(&self) {
//         let now = chrono::Utc::now().naive_utc();
//         let jobs = sqlx::query_as::<_, Job>(
//             "SELECT id, payload, scheduled_at FROM jobs WHERE scheduled_at <= $1 AND started_at IS NULL FOR UPDATE SKIP LOCKED",
//         )
//         .bind(&now)
//         .fetch_all(&self.pool)
//         .await
//         .unwrap();
//         for job in jobs {
//             let running_jobs = self.running_jobs.clone();
//             let pool = self.pool.clone();
//             let join_handle = tokio::spawn(async move {
//                 sqlx::query("UPDATE jobs SET started_at = $1 WHERE id = $2")
//                     .bind(&now)
//                     .bind(&job.id)
//                     .execute(&pool)
//                     .await
//                     .unwrap();
//                 // TODO: perform job
//                 sqlx::query("DELETE FROM jobs WHERE id = $1")
//                     .bind(&job.id)
//                     .execute(&pool)
//                     .await
//                     .unwrap();
//                 running_jobs.write().await.remove(&job.id);
//             });
//             self.running_jobs.write().await.insert(job.id, join_handle);
//         }
//     }

//     async fn delete_job(&self, job_id: i32) -> Result<(), sqlx::Error> {
//         sqlx::query(
//             "DELETE FROM jobs WHERE id = $1",
//         )
//         .bind(job_id)
//         .execute(&self.pool)
//         .await?;
//         Ok(())
//     }
// }