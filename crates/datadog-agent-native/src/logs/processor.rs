//! Log processing and transformation before aggregation.
//!
//! This module provides processing of raw log entries, including:
//! - **Parsing**: Converting raw log data into structured format
//! - **Enrichment**: Adding tags, metadata, and context
//! - **Filtering**: Applying inclusion/exclusion rules
//! - **Masking**: Redacting sensitive data based on regex patterns
//!
//! # Processing Rules
//!
//! The processor supports three types of rules:
//!
//! 1. **ExcludeAtMatch**: Drop logs matching a regex pattern
//! 2. **IncludeAtMatch**: Only keep logs matching a regex pattern
//! 3. **MaskSequences**: Replace matched patterns with placeholders
//!
//! # Architecture
//!
//! ```text
//!    Raw Log Event
//!         │
//!         v
//!   ┌──────────────┐
//!   │   Parser     │  (Extract message, timestamp, etc.)
//!   └──────┬───────┘
//!         │
//!         v
//!   ┌──────────────┐
//!   │   Enricher   │  (Add tags, metadata)
//!   └──────┬───────┘
//!         │
//!         v
//!   ┌──────────────┐
//!   │ Rule Engine  │  (Filter, mask)
//!   └──────┬───────┘
//!         │
//!         v
//!   ┌──────────────┐
//!   │  Aggregator  │  (Buffer for batching)
//!   └──────────────┘
//! ```
//!
//! # Rule Execution Order
//!
//! Rules are executed sequentially in the order they are defined:
//! 1. If any **ExcludeAtMatch** rule matches, log is dropped
//! 2. If any **IncludeAtMatch** rule doesn't match, log is dropped
//! 3. All **MaskSequences** rules are applied (replacements happen in order)

use std::sync::Arc;
use tokio::sync::mpsc::Sender;

use tracing::debug;

use crate::config::{self, processing_rule};
use crate::event_bus::Event;
// Lambda-specific imports removed for general agent
// use crate::extension::telemetry::events::TelemetryEvent;
// use crate::logs::lambda::processor::LambdaProcessor;
use crate::logs::agent::LogEvent;
use crate::logs::aggregator_service::AggregatorHandle;
use crate::tags;

impl LogsProcessor {
    /// Creates a new logs processor for the generic agent.
    ///
    /// This initializes a processor with configuration, tag provider,
    /// and event bus for publishing processing events.
    ///
    /// # Arguments
    ///
    /// * `config` - Shared agent configuration
    /// * `tags_provider` - Provider for enriching logs with tags
    /// * `event_bus` - Channel for publishing processing events
    ///
    /// # Returns
    ///
    /// A new [`LogsProcessor`] instance configured for generic log processing.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use datadog_agent_native::logs::processor::LogsProcessor;
    ///
    /// let processor = LogsProcessor::new(
    ///     Arc::clone(&config),
    ///     Arc::clone(&tags_provider),
    ///     event_bus,
    /// );
    /// ```
    #[must_use]
    pub fn new(
        config: Arc<config::Config>,
        tags_provider: Arc<tags::provider::Provider>,
        event_bus: Sender<Event>,
    ) -> Self {
        // Generic logs processor for general agent
        // In production, this would be expanded with proper log processing logic
        LogsProcessor::Generic {
            config,
            tags_provider,
            event_bus,
        }
    }

    /// Processes a log event and sends it to the aggregator.
    ///
    /// This is the main processing entry point. It:
    /// 1. Parses the raw log event
    /// 2. Applies processing rules (filter, mask)
    /// 3. Enriches with tags and metadata
    /// 4. Formats as JSON
    /// 5. Sends to aggregator for batching
    ///
    /// # Arguments
    ///
    /// * `event` - Raw log event to process
    /// * `_aggregator_handle` - Handle to send processed logs to aggregator
    ///
    /// # Implementation Note
    ///
    /// This is currently a stub implementation. In production, this would:
    /// - Extract and validate log fields
    /// - Apply configured processing rules
    /// - Add global and service-specific tags
    /// - Convert to Datadog's JSON log format
    /// - Send to aggregator via handle
    pub async fn process(&mut self, event: LogEvent, _aggregator_handle: &AggregatorHandle) {
        match self {
            LogsProcessor::Generic {
                config: _,
                tags_provider: _,
                event_bus: _,
            } => {
                // Stub implementation for generic log processing
                // In production, this would:
                // 1. Apply processing rules (see Processor trait)
                // 2. Add tags from tags_provider
                // 3. Format the log as JSON
                // 4. Send to aggregator via _aggregator_handle
                debug!("Processing log event: {:?}", event);
                // TODO: Implement generic log processing
            }
        }
    }
}

/// Log processor variants for different deployment environments.
///
/// This enum allows the processor to adapt its behavior based on the
/// deployment context (e.g., serverless vs. host-based agents).
///
/// # Variants
///
/// - **Generic**: Standard processor for host-based agents
///   - Uses configuration-based processing rules
///   - Enriches with host tags and metadata
///   - Supports custom tag providers
#[allow(clippy::module_name_repetitions)]
#[derive(Clone, Debug)]
pub enum LogsProcessor {
    // Lambda-specific variant removed for general agent
    // Lambda(LambdaProcessor),
    /// Generic log processor for host-based agents.
    ///
    /// This variant is used for standard agent deployments on VMs,
    /// containers, or other host-based environments.
    Generic {
        /// Shared agent configuration with processing rules.
        config: Arc<config::Config>,
        /// Provider for enriching logs with tags (service, env, version, etc.).
        tags_provider: Arc<tags::provider::Provider>,
        /// Event bus for publishing processing events (metrics, diagnostics).
        event_bus: Sender<Event>,
    },
}

/// Compiled processing rule for efficient log filtering and masking.
///
/// This struct represents a single processing rule after regex compilation.
/// Rules are applied to log messages in the order they are defined.
///
/// # Fields
///
/// - `kind`: Type of rule (exclude, include, or mask)
/// - `regex`: Compiled regular expression for pattern matching
/// - `placeholder`: Replacement text for mask rules (empty for filter rules)
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::logs::processor::Rule;
/// use datadog_agent_native::config::processing_rule::Kind;
///
/// let rule = Rule {
///     kind: Kind::MaskSequences,
///     regex: regex::Regex::new(r"\d{16}").unwrap(),
///     placeholder: "[CREDIT_CARD]".to_string(),
/// };
/// ```
#[derive(Clone, Debug)]
pub struct Rule {
    /// Type of processing rule.
    ///
    /// - **ExcludeAtMatch**: Drop logs matching this pattern
    /// - **IncludeAtMatch**: Only keep logs matching this pattern
    /// - **MaskSequences**: Replace matched patterns with placeholder
    pub kind: processing_rule::Kind,

    /// Compiled regex pattern for matching log content.
    pub regex: regex::Regex,

    /// Replacement text for mask rules.
    ///
    /// Used only for [`processing_rule::Kind::MaskSequences`] rules.
    /// Empty or ignored for filter rules.
    pub placeholder: String,
}

/// Trait for processing logs with filtering and masking rules.
///
/// This trait provides methods for compiling configuration rules into
/// efficient regex-based rules and applying them to log messages.
///
/// # Type Parameter
///
/// * `L` - The log type being processed (typically String)
///
/// # Rule Application Order
///
/// Rules are applied in definition order:
/// 1. **ExcludeAtMatch** rules checked first (early return on match)
/// 2. **IncludeAtMatch** rules checked next (early return on no match)
/// 3. **MaskSequences** rules applied to survivors (all replacements happen)
pub trait Processor<L> {
    /// Applies processing rules to a log message.
    ///
    /// This method executes all rules in order, potentially modifying the
    /// message (for mask rules) or determining if it should be dropped
    /// (for filter rules).
    ///
    /// # Arguments
    ///
    /// * `rules` - Optional list of compiled rules to apply
    /// * `message` - Mutable string to process (modified by mask rules)
    ///
    /// # Returns
    ///
    /// * `true` - Log should be kept (passed all filters, masks applied)
    /// * `false` - Log should be dropped (failed a filter rule)
    ///
    /// # Rule Behavior
    ///
    /// - **No rules**: Returns true (keep log)
    /// - **Empty rules**: Returns true (keep log)
    /// - **ExcludeAtMatch**: If pattern matches, return false (drop log)
    /// - **IncludeAtMatch**: If pattern doesn't match, return false (drop log)
    /// - **MaskSequences**: Replace all matches with placeholder (keep log)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use datadog_agent_native::logs::processor::{Processor, Rule};
    ///
    /// struct MyProcessor;
    /// impl Processor<String> for MyProcessor {}
    ///
    /// let rules = vec![
    ///     Rule {
    ///         kind: Kind::MaskSequences,
    ///         regex: Regex::new(r"\d{16}").unwrap(),
    ///         placeholder: "[REDACTED]".to_string(),
    ///     }
    /// ];
    ///
    /// let mut message = "Card: 1234567812345678".to_string();
    /// let keep = MyProcessor::apply_rules(&Some(rules), &mut message);
    /// // message = "Card: [REDACTED]", keep = true
    /// ```
    fn apply_rules(rules: &Option<Vec<Rule>>, message: &mut String) -> bool {
        match &rules {
            // No rules configured - keep log unchanged
            None => true,
            Some(rules) => {
                // Empty rules list - keep log unchanged
                if rules.is_empty() {
                    return true;
                }

                // Process rules in order
                for rule in rules {
                    match rule.kind {
                        // Exclude rule: Drop log if pattern matches
                        // Decision: Check early to avoid unnecessary processing
                        processing_rule::Kind::ExcludeAtMatch => {
                            if rule.regex.is_match(message) {
                                return false; // Drop this log
                            }
                        }
                        // Include rule: Drop log if pattern doesn't match
                        // Decision: Whitelist approach - only keep matching logs
                        processing_rule::Kind::IncludeAtMatch => {
                            if !rule.regex.is_match(message) {
                                return false; // Drop this log
                            }
                        }
                        // Mask rule: Replace all matches with placeholder
                        // Decision: Modify message in-place for efficiency
                        processing_rule::Kind::MaskSequences => {
                            *message = rule
                                .regex
                                .replace_all(message, rule.placeholder.as_str())
                                .to_string();
                        }
                    }
                }
                // All filter rules passed - keep log
                true
            }
        }
    }

    /// Compiles configuration rules into efficient regex-based rules.
    ///
    /// This method takes processing rules from configuration (with string
    /// patterns) and compiles them into [`Rule`] objects with compiled regexes
    /// for efficient matching.
    ///
    /// # Arguments
    ///
    /// * `rules` - Optional list of configuration rules to compile
    ///
    /// # Returns
    ///
    /// * `Some(Vec<Rule>)` - Successfully compiled rules (may be subset if some failed)
    /// * `None` - No rules provided or all compilation failed
    ///
    /// # Error Handling
    ///
    /// Invalid regex patterns are logged and skipped (not included in output).
    /// This allows the processor to continue with valid rules even if some are malformed.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use datadog_agent_native::logs::processor::Processor;
    /// use datadog_agent_native::config::processing_rule::{ProcessingRule, Kind};
    ///
    /// struct MyProcessor;
    /// impl Processor<String> for MyProcessor {}
    ///
    /// let config_rules = vec![
    ///     ProcessingRule {
    ///         kind: Kind::MaskSequences,
    ///         name: "credit_cards".to_string(),
    ///         pattern: r"\d{16}".to_string(),
    ///         replace_placeholder: Some("[CARD]".to_string()),
    ///     }
    /// ];
    ///
    /// let compiled = MyProcessor::compile_rules(&Some(config_rules));
    /// // compiled = Some(vec![Rule { ... }])
    /// ```
    fn compile_rules(
        rules: &Option<Vec<config::processing_rule::ProcessingRule>>,
    ) -> Option<Vec<Rule>> {
        match rules {
            // No rules configured
            None => None,
            Some(rules) => {
                // Empty rules list
                if rules.is_empty() {
                    return None;
                }

                let mut compiled_rules = Vec::new();

                // Compile each rule's regex pattern
                for rule in rules {
                    match regex::Regex::new(&rule.pattern) {
                        Ok(regex) => {
                            // Compilation succeeded - add to compiled rules
                            let placeholder = rule.replace_placeholder.clone().unwrap_or_default();
                            compiled_rules.push(Rule {
                                kind: rule.kind,
                                regex,
                                placeholder,
                            });
                        }
                        Err(e) => {
                            // Compilation failed - log and skip this rule
                            // Decision: Continue with other rules rather than failing entirely
                            debug!("Failed to compile rule '{}': {}", rule.name, e);
                        }
                    }
                }

                // Always return Some if rules were provided, even if all failed compilation
                // This distinguishes between "no rules configured" (None) and "all rules failed" (Some(vec![]))
                Some(compiled_rules)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct TestProcessor;
    impl Processor<String> for TestProcessor {}

    #[test]
    fn test_apply_rules_mask_sequences() {
        let rules = vec![Rule {
            kind: processing_rule::Kind::MaskSequences,
            regex: regex::Regex::new("replace-me").unwrap(),
            placeholder: "test-placeholder".to_string(),
        }];
        let mut message = "do-not-replace replace-me".to_string();

        let should_include = TestProcessor::apply_rules(&Some(rules), &mut message);
        assert!(should_include);
        assert_eq!(message, "do-not-replace test-placeholder");
    }

    #[test]
    fn test_apply_rules_exclude_at_match() {
        let rules = vec![Rule {
            kind: processing_rule::Kind::ExcludeAtMatch,
            regex: regex::Regex::new("exclude-me").unwrap(),
            placeholder: "test-placeholder".to_string(),
        }];
        let mut message = "exclude-me".to_string();

        let should_include = TestProcessor::apply_rules(&Some(rules), &mut message);
        assert!(!should_include);
    }

    #[test]
    fn test_apply_rules_include_at_match() {
        let rules = Some(vec![Rule {
            kind: processing_rule::Kind::IncludeAtMatch,
            regex: regex::Regex::new("include-me").unwrap(),
            placeholder: "test-placeholder".to_string(),
        }]);

        let mut message = "include-me".to_string();
        let should_include = TestProcessor::apply_rules(&rules, &mut message);
        assert!(should_include);

        let mut message = "do-not-include-me".to_string();
        let should_include = TestProcessor::apply_rules(&rules, &mut message);
        assert!(should_include);
    }

    #[test]
    fn test_compile_rules() {
        let rules = vec![processing_rule::ProcessingRule {
            kind: processing_rule::Kind::MaskSequences,
            name: "test".to_string(),
            pattern: "test-pattern".to_string(),
            replace_placeholder: Some("test-placeholder".to_string()),
        }];

        let compiled_rules = TestProcessor::compile_rules(&Some(rules));
        assert!(compiled_rules.is_some());
        assert_eq!(compiled_rules.unwrap().len(), 1);
    }

    #[test]
    fn test_compile_rules_empty() {
        let rules = vec![];

        let compiled_rules = TestProcessor::compile_rules(&Some(rules));
        assert!(compiled_rules.is_none());
    }

    #[test]
    fn test_compile_rules_none() {
        let compiled_rules = TestProcessor::compile_rules(&None);
        assert!(compiled_rules.is_none());
    }

    #[test]
    fn test_compile_rules_invalid_regex() {
        let rules = vec![processing_rule::ProcessingRule {
            kind: processing_rule::Kind::MaskSequences,
            name: "test".to_string(),
            pattern: "(".to_string(),
            replace_placeholder: Some("test-placeholder".to_string()),
        }];

        let compiled_rules = TestProcessor::compile_rules(&Some(rules));
        assert!(compiled_rules.is_some());
        assert_eq!(compiled_rules.unwrap().len(), 0);
    }
}
