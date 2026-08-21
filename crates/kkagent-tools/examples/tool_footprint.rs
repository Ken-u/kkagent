//! One-off analysis harness: measure per-tool schema footprint for the
//! default main-session tool set.
//!
//! Run with: cargo run -p kkagent-tools --example tool_footprint

use std::sync::Arc;

use kkagent_tools::builtin;
use kkagent_tools::web_providers::{WebSearchServiceConfig, WebServicesConfig};
use kkagent_tools::{register_core_tools, register_subagent_tools, ToolRegistry};

fn main() {
    let mut r = ToolRegistry::new();
    register_core_tools(&mut r);

    let mgr = Arc::new(kkagent_protocol::subagent::SubagentManager::new(4));
    let launch: builtin::task::SubagentLaunchFn = Arc::new(|_cfg| {});
    register_subagent_tools(&mut r, mgr, launch, None);

    let goal = Arc::new(kkagent_protocol::goal::GoalManager::new());
    r.register(Arc::new(builtin::GoalTool::new(goal)));

    r.register(Arc::new(builtin::SkillTool::new(Arc::new(
        builtin::skill::SkillCatalog::new(),
    ))));

    let web = Arc::new(WebServicesConfig {
        search: Some(WebSearchServiceConfig {
            provider: "demo".into(),
            base_url: "https://example.invalid/search".into(),
            api_key: None,
            timeout_ms: 10_000,
            default_limit: 5,
            proxy: Default::default(),
        }),
        fetch: Default::default(),
        migration_hint: None,
    });
    if let Some(t) = builtin::WebTool::try_new(web) {
        r.register(Arc::new(t));
    }

    let cron = Arc::new(builtin::cron::CronManager::default());
    r.register(Arc::new(builtin::CronTool::new(cron)));

    // --- report ---
    let defs = r.tool_definitions();
    println!("registered tools: {}", defs.len());

    struct Row {
        name: String,
        disclosure: kkagent_tools::ToolDisclosure,
        desc: usize,
        params: usize,
    }
    let mut rows: Vec<Row> = defs
        .iter()
        .map(|d| Row {
            name: d.name.clone(),
            disclosure: d.disclosure,
            desc: d.description.len(),
            params: d.parameters.to_string().len(),
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.desc + row.params));

    let mut inline = 0usize;
    let mut deferred_schema = 0usize;
    let mut deferred_names = 0usize;
    println!(
        "{:<18} {:>10} {:>7} {:>8} {:>8} {:>7}",
        "tool", "disclosure", "desc_ch", "param_ch", "sum_ch", "~tok"
    );
    for row in &rows {
        let sum = row.desc + row.params;
        let tag = match row.disclosure {
            kkagent_tools::ToolDisclosure::Inline => {
                inline += sum;
                "inline"
            }
            kkagent_tools::ToolDisclosure::Deferred => {
                deferred_schema += sum;
                deferred_names += row.name.len() + 1; // name + newline in announcement
                "deferred"
            }
        };
        // rough estimate: ~4 chars per token for JSON-ish English
        println!(
            "{:<18} {:>10} {:>7} {:>8} {:>8} {:>7}",
            row.name,
            tag,
            row.desc,
            row.params,
            sum,
            sum / 4
        );
    }
    println!();
    println!(
        "inline tools[] schema      : {inline} ch (~{} tok)",
        inline / 4
    );
    println!(
        "deferred announcement      : {deferred_names} ch (~{} tok)",
        deferred_names / 4
    );
    println!(
        "per-request total          : {} ch (~{} tok)",
        inline + deferred_names,
        (inline + deferred_names) / 4
    );
    println!(
        "saved vs all-inline        : {} ch (~{} tok)",
        deferred_schema - deferred_names,
        (deferred_schema - deferred_names) / 4
    );
}
