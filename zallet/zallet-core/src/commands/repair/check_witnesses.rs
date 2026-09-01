use abscissa_core::Runnable;
use nonempty::NonEmpty;
use zcash_client_backend::data_api::scanning::ScanPriority;

use crate::{
    cli::CheckWitnessesCmd,
    commands::AsyncRunnable,
    components::database::Database,
    error::{Error, ErrorKind},
    prelude::*,
};

impl AsyncRunnable for CheckWitnessesCmd {
    async fn run(&self) -> Result<(), Error> {
        let config = APP.config();
        let _lock = config.lock_datadir()?;

        let db = Database::open(&config).await?;
        let mut wallet = db.handle().await?;

        let ranges = wallet
            .check_witnesses()
            .map_err(|e| ErrorKind::Generic.context(e))?;

        for range in &ranges {
            println!("{}..{}", range.start, range.end);
        }

        match NonEmpty::from_vec(ranges) {
            None => info!("Every spendable note has a witness; nothing to rescan"),
            Some(ranges) if self.queue_rescan => {
                let count = ranges.len();
                wallet
                    // `Verify` is the priority the wallet uses for ranges it already scanned
                    // but no longer trusts, which is exactly what these are.
                    .queue_rescans(ranges, ScanPriority::Verify)
                    .map_err(|e| ErrorKind::Generic.context(e))?;
                info!("Queued {count} range(s) to be rescanned on the next sync");
            }
            Some(ranges) => info!(
                "{} range(s) need rescanning; re-run with --queue-rescan to queue them",
                ranges.len(),
            ),
        }

        Ok(())
    }
}

impl Runnable for CheckWitnessesCmd {
    fn run(&self) {
        self.run_on_runtime();
    }
}
