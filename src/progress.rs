use indicatif::{ProgressBar, ProgressStyle};

const CHECK_TEMPLATE: &str = "  {bar:30.cyan/dim} {pos}/{len} checked";

pub fn check_progress_bar(len: u64) -> ProgressBar {
    let pb = ProgressBar::new(len);
    let style = ProgressStyle::with_template(CHECK_TEMPLATE)
        .expect("progress template must be valid")
        .progress_chars("━╸─");
    pb.set_style(style);
    pb
}
