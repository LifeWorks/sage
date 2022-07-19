use carina::database::{IndexedDatabase, PeptideIx, Theoretical};
use carina::ion_series::Kind;
use carina::mass::{Tolerance, PROTON};
use carina::peptide::TargetDecoy;
use carina::spectrum::{read_spectrum, ProcessedSpectrum, SpectrumProcessor};
use clap::{Arg, Command};
use log::info;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{self, Instant};

#[derive(Copy, Clone)]
pub struct Score {
    peptide: PeptideIx,
    matched_b: u32,
    matched_y: u32,
    summed_b: f32,
    summed_y: f32,
    hyperlog: f32,
}

#[derive(Serialize)]
pub struct Percolator<'db> {
    peptide: String,
    proteins: &'db str,
    specid: usize,
    scannr: u32,
    label: i32,
    expmass: f32,
    calcmass: f32,
    charge: u8,
    rt: f32,
    delta_mass: f32,
    hyperscore: f32,
    deltascore: f32,
    matched_peaks: u32,
    summed_intensity: f32,
    total_candidates: usize,
    q_value: f32,
}

impl Score {
    fn hyperscore(&self, fact_table: &[f32]) -> f32 {
        let i = (self.summed_b + 1.0) * (self.summed_y + 1.0);
        let m = fact_table[(self.matched_b as usize).min(fact_table.len() - 1)]
            * fact_table[(self.matched_y as usize).min(fact_table.len() - 1)];
        let score = (i * m as f32).ln();
        if score.is_finite() {
            score
        } else {
            0.0
        }
    }

    pub fn new(peptide: &Theoretical) -> Self {
        Score {
            peptide: peptide.peptide_index,
            matched_b: 0,
            matched_y: 0,
            summed_b: 0.0,
            summed_y: 0.0,
            hyperlog: 0.0,
        }
    }
}

pub struct Scorer<'db> {
    db: &'db IndexedDatabase,
    search: &'db Search,
    factorial: [f32; 64],
}

impl<'db> Scorer<'db> {
    pub fn new(db: &'db IndexedDatabase, search: &'db Search) -> Self {
        let mut factorial = [1.0f32; 64];
        for i in 1..64 {
            factorial[i] = factorial[i - 1] * i as f32;
        }

        debug_assert!(factorial[3] == 6.0);
        debug_assert!(factorial[4] == 24.0);

        Scorer {
            db,
            search,
            factorial,
        }
    }

    pub fn score<'s>(&self, query: &ProcessedSpectrum) -> Vec<Percolator<'db>> {
        let mut scores: HashMap<PeptideIx, Score> = HashMap::new();
        let candidates = self
            .db
            .query(query, self.search.precursor_tol, self.search.fragment_tol);

        for (idx, fragment_mz) in query.mz.iter().enumerate() {
            // for charge in 1..=query.charge.min(self.search.max_fragment_charge) {
            // let fragment_mz = (fragment_mz * charge as f32) - (charge as f32 * PROTON);
            for frag in candidates.page_search(*fragment_mz) {
                let mut sc = scores
                    .entry(frag.peptide_index)
                    .or_insert_with(|| Score::new(frag));

                let intensity = query.int[idx];
                match frag.kind {
                    Kind::B => {
                        sc.matched_b += 1;
                        sc.summed_b += intensity
                    }
                    Kind::Y => {
                        sc.matched_y += 1;
                        sc.summed_y += intensity
                    }
                }
            }
            // }
        }

        let mut scores = scores
            .into_values()
            .filter(|sc| sc.matched_b + sc.matched_y > 0)
            .map(|mut sc| {
                sc.hyperlog = sc.hyperscore(&self.factorial);
                sc
            })
            .collect::<Vec<_>>();

        // FDR Calculation:
        // * Sort by score, descending
        // * Estimate FDR
        // * Calculate q-value

        scores.sort_by(|b, a| a.hyperlog.total_cmp(&b.hyperlog));
        let mut q_values = vec![0.0; scores.len()];
        let mut decoy = 1;
        let mut target = 0;

        for (idx, score) in scores.iter().enumerate() {
            match self.db[score.peptide] {
                TargetDecoy::Target(_) => target += 1,
                TargetDecoy::Decoy(_) => decoy += 1,
            }
            q_values[idx] = decoy as f32 / target as f32;
        }

        // Reverse array, and calculate the cumulative minimum
        let mut q_min = 1.0f32;
        for idx in (0..q_values.len()).rev() {
            q_min = q_min.min(q_values[idx]);
            q_values[idx] = q_min;
        }

        let mut reporting = Vec::new();
        if scores.is_empty() {
            return reporting;
        }
        for idx in 0..self.search.report_psms.min(scores.len()) {
            let better = scores[idx];
            let next = scores
                .get(idx + 1)
                .map(|score| score.hyperlog)
                .unwrap_or_default();

            let peptide = self.db[better.peptide].peptide();
            reporting.push(Percolator {
                peptide: peptide.to_string(),
                proteins: &peptide.protein,
                specid: 0,
                scannr: query.scan,
                label: self.db[better.peptide].label(),
                expmass: query.precursor_mz,
                calcmass: peptide.monoisotopic + PROTON,
                charge: query.charge,
                rt: query.rt,
                delta_mass: (query.precursor_mz - peptide.monoisotopic - PROTON),
                hyperscore: better.hyperlog,
                deltascore: better.hyperlog - next,
                matched_peaks: better.matched_b + better.matched_y,
                summed_intensity: better.summed_b + better.summed_y,
                total_candidates: scores.len(),
                q_value: q_values[idx],
            })
        }
        reporting
    }
}

#[derive(Serialize)]
pub struct Search {
    database: carina::database::Parameters,
    precursor_tol: Tolerance,
    fragment_tol: Tolerance,
    max_fragment_charge: u8,
    take_top_n: usize,
    report_psms: usize,
    ms2_paths: Vec<String>,
    pin_paths: Vec<String>,
    search_time: f32,
}

#[derive(Deserialize)]
struct Input {
    database: carina::database::Builder,
    precursor_tol: Tolerance,
    fragment_tol: Tolerance,
    report_psms: Option<usize>,
    take_top_n: Option<usize>,
    max_fragment_charge: Option<u8>,
    ms2_paths: Vec<String>,
}

impl Search {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let mut file = std::fs::File::open(path)?;
        let request: Input = serde_json::from_reader(&mut file)?;
        let database = request.database.make_parameters();
        Ok(Search {
            database,
            precursor_tol: request.precursor_tol,
            fragment_tol: request.fragment_tol,
            report_psms: request.report_psms.unwrap_or(1),
            take_top_n: request.take_top_n.unwrap_or(100),
            max_fragment_charge: request.max_fragment_charge.unwrap_or(3),
            pin_paths: Vec::new(),
            ms2_paths: request.ms2_paths,
            search_time: 0.0,
        })
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env = env_logger::Env::default().filter_or("CARINA_LOG", "info");
    env_logger::init_from_env(env);

    let start = time::Instant::now();

    let matches = Command::new("carina")
        .author("Michael Lazear <michaellazear92@gmail.com>")
        .arg(Arg::new("parameters").required(true))
        .get_matches();

    let path = matches
        .get_one::<String>("parameters")
        .expect("required parameters");
    let mut search = Search::load(path)?;

    let db = search.database.clone().build()?;
    info!(
        "generated {} fragments in {}ms",
        db.size(),
        (Instant::now() - start).as_millis()
    );

    let scorer = Scorer::new(&db, &search);
    let sp = SpectrumProcessor::new(
        search.take_top_n,
        search.max_fragment_charge,
        search.database.fragment_max_mz,
    );

    let mut pin_paths = Vec::with_capacity(search.ms2_paths.len());
    for ms2_path in &search.ms2_paths {
        let spectra = read_spectrum(ms2_path)?
            .into_iter()
            .map(|spectra| sp.process(spectra))
            .collect::<Vec<_>>();

        let start = Instant::now();
        let scores: Vec<Percolator> = spectra
            .par_iter()
            .flat_map(|spectra| scorer.score(spectra))
            .collect();
        let duration = Instant::now() - start;

        let pin_path = format!("{}.carina.pin", ms2_path);
        let mut writer = csv::WriterBuilder::new()
            .delimiter(b'\t')
            .from_path(&pin_path)?;

        for (idx, mut score) in scores.into_iter().enumerate() {
            score.specid = idx;
            writer.serialize(score)?;
        }

        pin_paths.push(pin_path);

        info!(
            "{:?}: processed {} scans in {} ms ({} scans/sec)",
            ms2_path,
            spectra.len(),
            duration.as_millis(),
            spectra.len() as f32 / duration.as_secs_f32()
        );
    }

    search.search_time = (Instant::now() - start).as_secs_f32();
    search.pin_paths = pin_paths;
    let results = serde_json::to_string_pretty(&search)?;

    eprintln!("{}", &results);
    std::fs::write("results.json", results)?;

    Ok(())
}