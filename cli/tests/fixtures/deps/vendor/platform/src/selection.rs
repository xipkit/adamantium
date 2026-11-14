std::cfg_select! {
    feature = "slow" => { mod missing_slow; }
    feature = "fast" => { mod selected; },
    _ => { mod missing_fallback; }
}

core::cfg_select! {
    platform_custom => { mod custom; }
    target_pointer_width = "8" => { mod missing_tiny; }
    _ => { mod portable; },
}
