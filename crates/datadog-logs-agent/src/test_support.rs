// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Helpers shared by this crate's unit tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts `warn!` and `error!` events emitted by this crate on the current thread.
#[derive(Clone, Default)]
pub(crate) struct LevelCounter {
    warnings: Arc<AtomicUsize>,
    errors: Arc<AtomicUsize>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LevelCounter {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        if !meta.target().starts_with("datadog_logs_agent") {
            return;
        }
        match *meta.level() {
            tracing::Level::WARN => self.warnings.fetch_add(1, Ordering::SeqCst),
            tracing::Level::ERROR => self.errors.fetch_add(1, Ordering::SeqCst),
            _ => return,
        };
    }
}

impl LevelCounter {
    /// Installs the counter for this thread. Use the current-thread runtime so tasks
    /// spawned by the test run on this thread.
    pub(crate) fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::layer::SubscriberExt as _;
        let counter = Self::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(counter.clone()));
        (counter, guard)
    }

    pub(crate) fn warnings(&self) -> usize {
        self.warnings.load(Ordering::SeqCst)
    }

    pub(crate) fn errors(&self) -> usize {
        self.errors.load(Ordering::SeqCst)
    }
}
