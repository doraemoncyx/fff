use fff_search::SnapshotStore;
use fff_search::binary_verdict::{BINARY_EXTENSIONS, TEXT_EXTENSIONS};
use fff_search::verdict_audit::{RootVerdicts, VerdictSample, root_verdicts};
use std::collections::BTreeMap;
use std::path::PathBuf;

// A bucket needs this many samples before it may decide a default entry.
const MIN_SAMPLES: u64 = 20;
const NO_EXTENSION: &str = "<none>";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A real tree's walk needs far more stack than the main thread's default
    // (a debug build overflows on a 23k-file tree); scans always ran on
    // dedicated threads, so the CLI keeps that property.
    let worker = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(|| audit().map_err(|e| e.to_string()))?;
    worker
        .join()
        .map_err(|_| "the audit thread panicked".to_string())?
        .map_err(Into::into)
}

fn audit() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    if !options.snapshot_db.is_dir() {
        return Err(format!(
            "no Snapshot environment at {}; point --snapshot-db at the service's --snapshot-db",
            options.snapshot_db.display()
        )
        .into());
    }

    let store = SnapshotStore::open(&options.snapshot_db)?;
    let mut audited: Vec<RootVerdicts> = Vec::with_capacity(options.roots.len());
    for root in &options.roots {
        let started = std::time::Instant::now();
        let verdicts = root_verdicts(&store, root)?;
        println!(
            "root {}: walked {} / joined {} / missing {} / rows {} / current {} ({:.2}s)",
            verdicts.root,
            verdicts.walked,
            verdicts.samples.len(),
            verdicts.walked - verdicts.samples.len(),
            verdicts.rows,
            verdicts.current,
            started.elapsed().as_secs_f64()
        );
        audited.push(verdicts);
    }

    let buckets = collect(&audited);
    std::fs::write(&options.out, tsv(&buckets))?;
    println!("{} buckets -> {}", buckets.len(), options.out.display());
    for line in report(&buckets) {
        println!("{line}");
    }
    Ok(())
}

#[derive(Default)]
struct Bucket {
    binary: u64,
    text: u64,
    // Root name to samples contributed, so the biggest source is nameable.
    by_root: BTreeMap<String, u64>,
}

impl Bucket {
    fn total(&self) -> u64 {
        self.binary + self.text
    }

    fn mixed_use(&self) -> bool {
        self.binary > 0 && self.text > 0
    }

    fn biggest_root(&self) -> &str {
        self.by_root
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map_or("", |(root, _)| root.as_str())
    }
}

struct Options {
    snapshot_db: PathBuf,
    out: PathBuf,
    roots: Vec<PathBuf>,
}

impl Options {
    fn parse() -> Result<Self, Box<dyn std::error::Error>> {
        let mut snapshot_db = None;
        let mut out = None;
        let mut roots = Vec::new();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--snapshot-db" => {
                    snapshot_db = Some(args.next().ok_or("--snapshot-db needs a path")?)
                }
                "--out" => out = Some(args.next().ok_or("--out needs a path")?),
                "--help" | "-h" => {
                    println!(
                        "usage: verdict_audit --snapshot-db <DIR> --out <TSV> <root> [<root>...]"
                    );
                    std::process::exit(0);
                }
                root => roots.push(PathBuf::from(root)),
            }
        }
        let (Some(snapshot_db), Some(out)) = (snapshot_db, out) else {
            return Err("--snapshot-db and --out are required; --help for usage".into());
        };
        if roots.is_empty() {
            return Err("name at least one root to audit; --help for usage".into());
        }
        Ok(Self {
            snapshot_db: PathBuf::from(snapshot_db),
            out: PathBuf::from(out),
            roots,
        })
    }
}

fn collect(roots: &[RootVerdicts]) -> BTreeMap<Option<String>, Bucket> {
    let mut buckets: BTreeMap<Option<String>, Bucket> = BTreeMap::new();
    for root in roots {
        for VerdictSample { extension, binary } in &root.samples {
            let bucket = buckets.entry(extension.clone()).or_default();
            if *binary {
                bucket.binary += 1;
            } else {
                bucket.text += 1;
            }
            *bucket.by_root.entry(root.root.clone()).or_insert(0) += 1;
        }
    }
    buckets
}

// Two runs of `collect` differ only in these numbers, which is what makes the
// tsv diffable across scans.
fn tsv(buckets: &BTreeMap<Option<String>, Bucket>) -> String {
    let mut rows: Vec<(&Option<String>, &Bucket)> = buckets.iter().collect();
    rows.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then_with(|| a.0.cmp(b.0)));

    let mut out = String::from("extension\tbinary\ttext\tbiggest_root\n");
    for (extension, bucket) in rows {
        out.push_str(extension.as_deref().unwrap_or(NO_EXTENSION));
        out.push('\t');
        out.push_str(&bucket.binary.to_string());
        out.push('\t');
        out.push_str(&bucket.text.to_string());
        out.push('\t');
        out.push_str(bucket.biggest_root());
        out.push('\n');
    }
    out
}

fn report(buckets: &BTreeMap<Option<String>, Bucket>) -> Vec<String> {
    let label = |extension: &Option<String>| {
        extension
            .clone()
            .unwrap_or_else(|| NO_EXTENSION.to_string())
    };
    let on = |table: &[&str], extension: &Option<String>| {
        extension
            .as_deref()
            .is_some_and(|ext| table.binary_search(&ext).is_ok())
    };
    let sorted = |keep: &dyn Fn(&Option<String>, &Bucket) -> bool| {
        let mut picked: Vec<(&Option<String>, &Bucket)> = buckets
            .iter()
            .filter(|(extension, bucket)| keep(extension, bucket))
            .collect();
        picked.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then_with(|| a.0.cmp(b.0)));
        picked
    };
    let entries = |picked: Vec<(&Option<String>, &Bucket)>| -> Vec<String> {
        picked
            .into_iter()
            .map(|(extension, bucket)| {
                format!("{}({}/{})", label(extension), bucket.binary, bucket.text)
            })
            .collect()
    };

    let samples: u64 = buckets.values().map(Bucket::total).sum();
    let mut lines = vec![format!("{} buckets, {samples} samples", buckets.len())];

    let mixed = entries(sorted(&|_, bucket| bucket.mixed_use()));
    lines.push(format!(
        "mixed-use, byte path stays ({}): {}",
        mixed.len(),
        join(mixed)
    ));

    let confirmed = entries(sorted(&|extension, _| on(BINARY_EXTENSIONS, extension)));
    lines.push(format!(
        "already on the built-in binary table ({}): {}",
        confirmed.len(),
        join(confirmed)
    ));

    let candidates = entries(sorted(&|extension, bucket| {
        bucket.total() >= MIN_SAMPLES
            && bucket.text == 0
            && !on(BINARY_EXTENSIONS, extension)
            && !on(TEXT_EXTENSIONS, extension)
    }));
    lines.push(format!(
        "default-table candidates (>={MIN_SAMPLES} samples, no text, no table) ({}): {}",
        candidates.len(),
        join(candidates)
    ));

    if let Some(bucket) = buckets.get(&None) {
        lines.push(format!(
            "no extension: binary {} / text {}",
            bucket.binary, bucket.text
        ));
    }
    lines
}

fn join(entries: Vec<String>) -> String {
    if entries.is_empty() {
        "none".to_string()
    } else {
        entries.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(extension: Option<&str>, binary: bool) -> VerdictSample {
        VerdictSample {
            extension: extension.map(str::to_string),
            binary,
        }
    }

    fn root(name: &str, samples: Vec<VerdictSample>) -> RootVerdicts {
        RootVerdicts {
            root: name.to_string(),
            walked: samples.len(),
            samples,
            rows: 0,
            current: true,
        }
    }

    #[test]
    fn a_tsv_row_carries_the_four_measured_fields() {
        let buckets = collect(&[
            root("a", vec![sample(Some("uasset"), true)]),
            root("b", vec![sample(Some("uasset"), true)]),
            root(
                "b",
                vec![sample(Some("mtl"), false), sample(Some("mtl"), true)],
            ),
        ]);
        let tsv = tsv(&buckets);

        let mut lines = tsv.lines();
        assert_eq!(lines.next(), Some("extension\tbinary\ttext\tbiggest_root"));
        // Equal totals sort by extension, so `mtl` comes before `uasset`;
        // `uasset`'s two samples are one per root, and the tie names `a`.
        assert_eq!(lines.next(), Some("mtl\t1\t1\tb"));
        assert_eq!(lines.next(), Some("uasset\t2\t0\ta"));
        assert_eq!(lines.next(), None);
    }

    // The biggest source is the root that contributed the most samples, and a
    // tie resolves by name so two runs print the same line.
    #[test]
    fn the_biggest_root_breaks_ties_by_name() {
        let buckets = collect(&[
            root(
                "zzz",
                vec![sample(Some("png"), true), sample(Some("png"), true)],
            ),
            root(
                "aaa",
                vec![sample(Some("png"), true), sample(Some("png"), false)],
            ),
            root("mmm", vec![sample(Some("png"), true)]),
        ]);
        assert_eq!(buckets[&Some("png".to_string())].biggest_root(), "aaa");
    }

    #[test]
    fn the_report_separates_mixed_use_from_candidates() {
        let mut samples = Vec::new();
        for _ in 0..MIN_SAMPLES {
            samples.push(sample(Some("umap"), true));
        }
        samples.push(sample(Some("atlas"), true));
        samples.push(sample(Some("atlas"), false));
        for _ in 0..MIN_SAMPLES {
            samples.push(sample(Some("png"), true));
        }
        samples.push(sample(None, true));
        let buckets = collect(&[root("tree", samples)]);
        let report = report(&buckets).join("\n");
        let candidates = format!("candidates (>={MIN_SAMPLES} samples, no text, no table) (1)");

        assert!(
            report.contains("mixed-use, byte path stays (1): atlas(1/1)"),
            "{report}"
        );
        assert!(report.contains(&candidates), "{report}");
        assert!(report.contains("umap(20/0)"), "{report}");
        assert!(
            report.contains("already on the built-in binary table (1): png(20/0)"),
            "{report}"
        );
        assert!(
            report.contains("no extension: binary 1 / text 0"),
            "{report}"
        );
    }

    // A bucket below the threshold is measured but never proposed.
    #[test]
    fn a_thin_bucket_is_not_a_candidate() {
        let buckets = collect(&[root(
            "tree",
            (0..MIN_SAMPLES - 1)
                .map(|_| sample(Some("tiny"), true))
                .collect(),
        )]);
        let report = report(&buckets).join("\n");
        let none = format!("candidates (>={MIN_SAMPLES} samples, no text, no table) (0): none");
        assert!(report.contains(&none), "{report}");
    }
}
