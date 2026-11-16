//! Deterministic synthetic vault generator for tests and benchmarks.

#![allow(dead_code)]

use std::path::Path;

const WORDS: &[&str] = &[
    "lunar", "orbit", "garden", "harbor", "signal", "copper", "meadow", "lantern", "river",
    "quartz", "ember", "thicket", "compass", "glacier", "pilgrim", "saffron", "tundra", "violet",
    "willow", "zephyr", "anchor", "basalt", "cedar", "delta", "ferric", "granite", "hollow",
    "indigo", "juniper", "kestrel", "lichen", "marble", "nectar", "obsidian", "pebble", "quill",
    "raven", "sable", "timber", "umber", "vessel", "wander", "yarrow", "zenith", "atlas", "beacon",
    "canyon", "dune", "estuary", "fjord", "grove", "heath", "isle", "jetty", "knoll", "lagoon",
    "mesa", "nook", "oasis", "prairie", "quarry", "ridge", "summit", "trail", "upland", "valley",
];

/// Small linear congruential generator (deterministic, dependency-free).
pub struct Lcg(u64);

impl Lcg {
    /// Seeded generator.
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6364136223846793005).wrapping_add(1))
    }
    /// Next value in `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

/// Note name for index `i`.
pub fn note_name(i: usize) -> String {
    format!("Note {i:05}")
}

/// Write `n` notes (about 120 words, 3 wikilinks and 2 tags each) into `root/notes/<bucket>/`.
pub fn generate_vault(root: &Path, n: usize) -> std::io::Result<()> {
    let mut rng = Lcg::new(42);
    for i in 0..n {
        let dir = root.join("notes").join(format!("b{:02}", i % 50));
        std::fs::create_dir_all(&dir)?;
        let mut body = String::new();
        body.push_str(&format!(
            "---\ntags: [{}]\n---\n# {}\n\n",
            WORDS[rng.below(WORDS.len())],
            note_name(i)
        ));
        for line in 0..6 {
            for _ in 0..20 {
                body.push_str(WORDS[rng.below(WORDS.len())]);
                body.push(' ');
            }
            if line < 3 {
                body.push_str(&format!("[[{}]] ", note_name(rng.below(n))));
            }
            body.push_str(&format!("#{}\n", WORDS[rng.below(WORDS.len())]));
        }
        std::fs::write(dir.join(format!("{}.md", note_name(i))), body)?;
    }
    Ok(())
}
