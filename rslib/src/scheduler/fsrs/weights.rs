// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html
use std::collections::HashMap;
use std::iter;
use std::path::Path;
use std::thread;
use std::time::Duration;

use anki_io::write_file;
use anki_proto::scheduler::ComputeFsrsWeightsResponse;
use anki_proto::stats::revlog_entry;
use anki_proto::stats::Dataset;
use anki_proto::stats::DeckEntry;
use chrono::NaiveDate;
use chrono::NaiveTime;
use fsrs::CombinedProgressState;
use fsrs::FSRSItem;
use fsrs::FSRSReview;
use fsrs::ModelEvaluation;
use fsrs::FSRS;
use itertools::Itertools;
use prost::Message;

use crate::decks::immediate_parent_name;
use crate::prelude::*;
use crate::revlog::RevlogEntry;
use crate::revlog::RevlogReviewKind;
use crate::search::Node;
use crate::search::SearchNode;
use crate::search::SortMode;

pub(crate) type Weights = Vec<f32>;

fn ignore_revlogs_before_date_to_ms(
    ignore_revlogs_before_date: &String,
) -> Result<TimestampMillis> {
    Ok(match ignore_revlogs_before_date {
        s if s.is_empty() => 0,
        s => NaiveDate::parse_from_str(s.as_str(), "%Y-%m-%d")
            .or_else(|err| invalid_input!(err, "Error parsing date: {s}"))?
            .and_time(NaiveTime::from_hms_milli_opt(0, 0, 0, 0).unwrap())
            .and_utc()
            .timestamp_millis(),
    }
    .into())
}

pub(crate) fn ignore_revlogs_before_ms_from_config(config: &DeckConfig) -> Result<TimestampMillis> {
    ignore_revlogs_before_date_to_ms(&config.inner.ignore_revlogs_before_date)
}

impl Collection {
    /// Note this does not return an error if there are less than 400 items -
    /// the caller should instead check the fsrs_items count in the return
    /// value.
    pub fn compute_weights(
        &mut self,
        search: &str,
        ignore_revlogs_before: TimestampMillis,
        current_preset: u32,
        total_presets: u32,
        current_weights: &Weights,
    ) -> Result<ComputeFsrsWeightsResponse> {
        let mut anki_progress = self.new_progress_handler::<ComputeWeightsProgress>();
        let timing = self.timing_today()?;
        let revlogs = self.revlog_for_srs(search)?;
        let (items, review_count) =
            fsrs_items_for_training(revlogs.clone(), timing.next_day_at, ignore_revlogs_before);

        let fsrs_items = items.len() as u32;
        if fsrs_items == 0 {
            return Ok(ComputeFsrsWeightsResponse {
                weights: current_weights.to_vec(),
                fsrs_items,
            });
        }
        anki_progress.update(false, |p| {
            p.current_preset = current_preset;
            p.total_presets = total_presets;
        })?;
        // adapt the progress handler to our built-in progress handling
        let progress = CombinedProgressState::new_shared();
        let progress2 = progress.clone();
        thread::spawn(move || {
            let mut finished = false;
            while !finished {
                thread::sleep(Duration::from_millis(100));
                let mut guard = progress.lock().unwrap();
                if let Err(_err) = anki_progress.update(false, |s| {
                    s.total_iterations = guard.total() as u32;
                    s.current_iteration = guard.current() as u32;
                    s.reviews = review_count as u32;
                    finished = guard.finished();
                }) {
                    guard.want_abort = true;
                    return;
                }
            }
        });
        let fsrs = FSRS::new(Some(current_weights))?;
        let current_rmse = fsrs.evaluate(items.clone(), |_| true)?.rmse_bins;
        let mut weights = fsrs.compute_parameters(items.clone(), Some(progress2))?;
        let optimized_fsrs = FSRS::new(Some(&weights))?;
        let optimized_rmse = optimized_fsrs.evaluate(items.clone(), |_| true)?.rmse_bins;
        if current_rmse <= optimized_rmse {
            weights = current_weights.to_vec();
        }

        Ok(ComputeFsrsWeightsResponse {
            weights,
            fsrs_items,
        })
    }

    pub(crate) fn revlog_for_srs(
        &mut self,
        search: impl TryIntoSearch,
    ) -> Result<Vec<RevlogEntry>> {
        let search = search.try_into_search()?;
        // a whole-collection search can match revlog entries of deleted cards, too
        if let Node::Group(nodes) = &search {
            if let &[Node::Search(SearchNode::WholeCollection)] = &nodes[..] {
                return self.storage.get_all_revlog_entries_in_card_order();
            }
        }
        self.search_cards_into_table(search, SortMode::NoOrder)?
            .col
            .storage
            .get_revlog_entries_for_searched_cards_in_card_order()
    }

    /// Used for exporting revlogs for algorithm research.
    pub fn export_dataset(&mut self, min_entries: usize, target_path: &Path) -> Result<()> {
        let revlog_entries = self.storage.get_all_revlog_entries_in_card_order()?;
        if revlog_entries.len() < min_entries {
            return Err(AnkiError::FsrsInsufficientData);
        }
        let revlogs = revlog_entries
            .into_iter()
            .map(revlog_entry_to_proto)
            .collect_vec();
        let cards = self.storage.get_all_card_entries()?;

        let decks_map = self.storage.get_decks_map()?;
        let deck_name_to_id: HashMap<String, DeckId> = decks_map
            .into_iter()
            .map(|(id, deck)| (deck.name.to_string(), id))
            .collect();

        let decks = self
            .storage
            .get_all_decks()?
            .into_iter()
            .filter_map(|deck| {
                if let Some(preset_id) = deck.config_id().map(|id| id.0) {
                    let parent_id = immediate_parent_name(&deck.name.to_string())
                        .and_then(|parent_name| deck_name_to_id.get(parent_name))
                        .map(|id| id.0)
                        .unwrap_or(0);
                    Some(DeckEntry {
                        id: deck.id.0,
                        parent_id,
                        preset_id,
                    })
                } else {
                    None
                }
            })
            .collect_vec();
        let next_day_at = self.timing_today()?.next_day_at.0;
        let dataset = Dataset {
            revlogs,
            cards,
            decks,
            next_day_at,
        };
        let data = dataset.encode_to_vec();
        write_file(target_path, data)?;
        Ok(())
    }

    pub fn evaluate_weights(
        &mut self,
        weights: &Weights,
        search: &str,
        ignore_revlogs_before: TimestampMillis,
    ) -> Result<ModelEvaluation> {
        let timing = self.timing_today()?;
        let mut anki_progress = self.new_progress_handler::<ComputeWeightsProgress>();
        let guard = self.search_cards_into_table(search, SortMode::NoOrder)?;
        let revlogs: Vec<RevlogEntry> = guard
            .col
            .storage
            .get_revlog_entries_for_searched_cards_in_card_order()?;
        let (items, review_count) =
            fsrs_items_for_training(revlogs, timing.next_day_at, ignore_revlogs_before);
        anki_progress.state.reviews = review_count as u32;
        let fsrs = FSRS::new(Some(weights))?;
        Ok(fsrs.evaluate(items, |ip| {
            anki_progress
                .update(false, |p| {
                    p.total_iterations = ip.total as u32;
                    p.current_iteration = ip.current as u32;
                })
                .is_ok()
        })?)
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct ComputeWeightsProgress {
    pub current_iteration: u32,
    pub total_iterations: u32,
    pub reviews: u32,
    /// Only used in 'compute all weights' case
    pub current_preset: u32,
    /// Only used in 'compute all weights' case
    pub total_presets: u32,
}

/// Convert a series of revlog entries sorted by card id into FSRS items.
fn fsrs_items_for_training(
    revlogs: Vec<RevlogEntry>,
    next_day_at: TimestampSecs,
    review_revlogs_before: TimestampMillis,
) -> (Vec<FSRSItem>, usize) {
    let mut review_count: usize = 0;
    let mut revlogs = revlogs
        .into_iter()
        .chunk_by(|r| r.cid)
        .into_iter()
        .filter_map(|(_cid, entries)| {
            single_card_revlog_to_items(entries.collect(), next_day_at, true, review_revlogs_before)
        })
        .flat_map(|i| {
            review_count += i.2;

            i.0
        })
        .collect_vec();
    revlogs.sort_by_cached_key(|r| r.reviews.len());
    (revlogs, review_count)
}

/// Transform the revlog history for a card into a list of FSRSItems. FSRS
/// expects multiple items for a given card when training - for revlog
/// `[1,2,3]`, we create FSRSItems corresponding to `[1,2]` and `[1,2,3]`
/// in training, and `[1]`, [1,2]` and `[1,2,3]` when calculating memory
/// state.
///
/// Returns (items, revlog_complete, review_count).
/// revlog_complete is assumed when the revlogs have a learning step, or start
/// with manual scheduling. When revlogs are incomplete, the starting difficulty
/// is later inferred from the SM2 data, instead of using the standard FSRS
/// initial difficulty. review_count is the number of reviews used after
/// filtering out unwanted ones.
pub(crate) fn single_card_revlog_to_items(
    mut entries: Vec<RevlogEntry>,
    next_day_at: TimestampSecs,
    training: bool,
    ignore_revlogs_before: TimestampMillis,
) -> Option<(Vec<FSRSItem>, bool, usize)> {
    let mut first_of_last_learn_entries = None;
    let mut revlogs_complete = false;
    for (index, entry) in entries.iter().enumerate().rev() {
        if matches!(
            (entry.review_kind, entry.button_chosen),
            (RevlogReviewKind::Learning, 1..=4)
        ) {
            first_of_last_learn_entries = Some(index);
            revlogs_complete = true;
        } else if first_of_last_learn_entries.is_some() {
            break;
        } else if matches!(
            (entry.review_kind, entry.ease_factor),
            (RevlogReviewKind::Manual, 0)
        ) {
            // If we find a `Learn` entry after the `Forget` entry, we should
            // ignore the entries before the `Forget` entry
            if first_of_last_learn_entries.is_some() {
                revlogs_complete = false;
                break;
            // If we don't find a `Learn` entry after the `Forget` entry, it's
            // a new card and we should ignore all entries
            } else {
                return None;
            }
        }
    }
    if !revlogs_complete {
        revlogs_complete = matches!(
            entries.first(),
            Some(RevlogEntry {
                review_kind: RevlogReviewKind::Manual,
                ..
            })
        );
    }
    if training {
        // While training ignore the entire card if the first learning step of the last
        // group of learning steps is before the ignore_revlogs_before date
        if let Some(idx) = first_of_last_learn_entries {
            if entries[idx].id.0 < ignore_revlogs_before.0 {
                return None;
            }
        }
    } else {
        // While reviewing if the first learning step is before the ignore date,
        // ignore every review before and including the last learning step
        if let Some(idx) = first_of_last_learn_entries {
            if entries[idx].id.0 < ignore_revlogs_before.0 && idx < entries.len() - 1 {
                let last_learn_entry = entries
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_idx, e)| e.review_kind == RevlogReviewKind::Learning)
                    .map(|(idx, _)| idx);

                entries.drain(..(last_learn_entry? + 1));
                revlogs_complete = false;
                first_of_last_learn_entries = None;
            }
        }
    }
    let first_relearn = entries
        .iter()
        .enumerate()
        .find(|(_idx, e)| {
            e.id.0 > ignore_revlogs_before.0 && e.review_kind == RevlogReviewKind::Relearning
        })
        .map(|(idx, _)| idx);
    if let Some(idx) = first_of_last_learn_entries.or(first_relearn) {
        // start from the (re)learning step
        if idx > 0 {
            entries.drain(..idx);
        }
    } else if training {
        // when training, we ignore cards that don't have any learning steps
        return None;
    }

    // Filter out unwanted entries
    entries.retain(|entry| {
        !(
            // manually rescheduled
            (entry.review_kind == RevlogReviewKind::Manual || entry.button_chosen == 0)
            || // cram
            (entry.review_kind == RevlogReviewKind::Filtered && entry.ease_factor == 0)
        )
    });

    // Compute delta_t for each entry
    let delta_ts = iter::once(0)
        .chain(entries.iter().tuple_windows().map(|(previous, current)| {
            previous.days_elapsed(next_day_at) - current.days_elapsed(next_day_at)
        }))
        .collect_vec();

    let skip = if training { 1 } else { 0 };
    // Convert the remaining entries into separate FSRSItems, where each item
    // contains all reviews done until then.
    let items: Vec<FSRSItem> = entries
        .iter()
        .enumerate()
        .skip(skip)
        .map(|(outer_idx, _)| {
            let reviews = entries
                .iter()
                .take(outer_idx + 1)
                .enumerate()
                .map(|(inner_idx, r)| FSRSReview {
                    rating: r.button_chosen as u32,
                    delta_t: delta_ts[inner_idx],
                })
                .collect();
            FSRSItem { reviews }
        })
        .filter(|item| !training || item.reviews.last().unwrap().delta_t > 0)
        .collect_vec();
    if items.is_empty() {
        None
    } else {
        Some((items, revlogs_complete, entries.len()))
    }
}

impl RevlogEntry {
    fn days_elapsed(&self, next_day_at: TimestampSecs) -> u32 {
        (next_day_at.elapsed_secs_since(self.id.as_secs()) / 86_400).max(0) as u32
    }
}

fn revlog_entry_to_proto(e: RevlogEntry) -> anki_proto::stats::RevlogEntry {
    anki_proto::stats::RevlogEntry {
        id: e.id.0,
        cid: e.cid.0,
        usn: 0,
        button_chosen: e.button_chosen as u32,
        interval: e.interval,
        last_interval: e.last_interval,
        ease_factor: e.ease_factor,
        taken_millis: e.taken_millis,
        review_kind: match e.review_kind {
            RevlogReviewKind::Learning => revlog_entry::ReviewKind::Learning,
            RevlogReviewKind::Review => revlog_entry::ReviewKind::Review,
            RevlogReviewKind::Relearning => revlog_entry::ReviewKind::Relearning,
            RevlogReviewKind::Filtered => revlog_entry::ReviewKind::Filtered,
            RevlogReviewKind::Manual => revlog_entry::ReviewKind::Manual,
        } as i32,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const NEXT_DAY_AT: TimestampSecs = TimestampSecs(86400 * 100);

    fn days_ago_ms(days_ago: i64) -> TimestampMillis {
        ((NEXT_DAY_AT.0 - days_ago * 86400) * 1000).into()
    }

    pub(crate) fn revlog(review_kind: RevlogReviewKind, days_ago: i64) -> RevlogEntry {
        RevlogEntry {
            review_kind,
            id: days_ago_ms(days_ago).into(),
            button_chosen: 3,
            ..Default::default()
        }
    }

    pub(crate) fn review(delta_t: u32) -> FSRSReview {
        FSRSReview { rating: 3, delta_t }
    }

    pub(crate) fn convert_ignore_before(
        revlog: &[RevlogEntry],
        training: bool,
        ignore_before: TimestampMillis,
    ) -> Option<Vec<FSRSItem>> {
        single_card_revlog_to_items(revlog.to_vec(), NEXT_DAY_AT, training, ignore_before)
            .map(|i| i.0)
    }

    pub(crate) fn convert(revlog: &[RevlogEntry], training: bool) -> Option<Vec<FSRSItem>> {
        convert_ignore_before(revlog, training, 0.into())
    }

    #[macro_export]
    macro_rules! fsrs_items {
        ($($reviews:expr),*) => {
            Some(vec![
                $(
                    FSRSItem {
                        reviews: $reviews.to_vec()
                    }
                ),*
            ])
        };
    }

    pub(crate) use fsrs_items;

    #[test]
    fn delta_t_is_correct() -> Result<()> {
        assert_eq!(
            convert(
                &[
                    revlog(RevlogReviewKind::Learning, 1),
                    revlog(RevlogReviewKind::Review, 0)
                ],
                true,
            ),
            fsrs_items!([review(0), review(1)])
        );
        assert_eq!(
            convert(
                &[
                    revlog(RevlogReviewKind::Learning, 15),
                    revlog(RevlogReviewKind::Learning, 13),
                    revlog(RevlogReviewKind::Review, 10),
                    revlog(RevlogReviewKind::Review, 5)
                ],
                true,
            ),
            fsrs_items!(
                [review(0), review(2)],
                [review(0), review(2), review(3)],
                [review(0), review(2), review(3), review(5)]
            )
        );
        assert_eq!(
            convert(
                &[
                    revlog(RevlogReviewKind::Learning, 15),
                    revlog(RevlogReviewKind::Learning, 13),
                ],
                true,
            ),
            fsrs_items!([review(0), review(2),])
        );
        Ok(())
    }

    #[test]
    fn cram_is_filtered() {
        assert_eq!(
            convert(
                &[
                    revlog(RevlogReviewKind::Learning, 10),
                    revlog(RevlogReviewKind::Review, 9),
                    revlog(RevlogReviewKind::Filtered, 7),
                    revlog(RevlogReviewKind::Review, 4),
                ],
                true,
            ),
            fsrs_items!([review(0), review(1)], [review(0), review(1), review(5)])
        );
    }

    #[test]
    fn set_due_date_is_filtered() {
        assert_eq!(
            convert(
                &[
                    revlog(RevlogReviewKind::Learning, 10),
                    revlog(RevlogReviewKind::Review, 9),
                    RevlogEntry {
                        ease_factor: 100,
                        ..revlog(RevlogReviewKind::Manual, 7)
                    },
                    revlog(RevlogReviewKind::Review, 4),
                ],
                true,
            ),
            fsrs_items!([review(0), review(1)], [review(0), review(1), review(5)])
        );
    }

    #[test]
    fn card_reset_drops_all_previous_history() {
        assert_eq!(
            convert(
                &[
                    revlog(RevlogReviewKind::Learning, 10),
                    revlog(RevlogReviewKind::Review, 9),
                    RevlogEntry {
                        ease_factor: 0,
                        ..revlog(RevlogReviewKind::Manual, 7)
                    },
                    revlog(RevlogReviewKind::Learning, 4),
                    revlog(RevlogReviewKind::Review, 0),
                ],
                true,
            ),
            fsrs_items!([review(0), review(4)])
        );
    }

    #[test]
    fn single_learning_step_skipped_when_training() {
        assert_eq!(
            convert(&[revlog(RevlogReviewKind::Learning, 1),], true),
            None,
        );
        assert_eq!(
            convert(&[revlog(RevlogReviewKind::Learning, 1),], false),
            fsrs_items!([review(0)])
        );
    }

    #[test]
    fn ignores_cards_before_ignore_before_date_when_training() {
        let revlogs = &[
            revlog(RevlogReviewKind::Learning, 10),
            revlog(RevlogReviewKind::Learning, 8),
        ];
        // | = Ignore before
        // L = learning step
        // L L |
        assert_eq!(convert_ignore_before(revlogs, true, days_ago_ms(7)), None);
        // L | L
        assert_eq!(convert_ignore_before(revlogs, true, days_ago_ms(9)), None);
        // L (|L) (exact same millisecond)
        assert_eq!(
            convert_ignore_before(revlogs, true, days_ago_ms(10)),
            convert(revlogs, true)
        );
        // | L L
        assert_eq!(
            convert_ignore_before(revlogs, true, days_ago_ms(11)),
            convert(revlogs, true)
        );
    }

    #[test]
    fn ignore_before_date_between_learning_steps_when_reviewing() {
        let revlogs = &[
            revlog(RevlogReviewKind::Learning, 10),
            revlog(RevlogReviewKind::Learning, 8),
            revlog(RevlogReviewKind::Review, 2),
        ];
        // L | L R
        assert_ne!(
            convert_ignore_before(revlogs, false, days_ago_ms(9)),
            convert(revlogs, false)
        );
        assert_eq!(
            convert_ignore_before(revlogs, false, days_ago_ms(9))
                .unwrap()
                .len(),
            1
        );
        // | L L R
        assert_eq!(
            convert_ignore_before(revlogs, false, days_ago_ms(11)),
            convert(revlogs, false)
        );
    }
}
