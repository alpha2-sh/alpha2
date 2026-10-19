use super::{FrameStore, MAX_IMAGES, MAX_RETIRED, MAX_VIDEO_FRAMES, WorkingSetOverBudget};

/// One `Rgba16Float` 4K upload.
const FRAME_4K: u64 = 3840 * 2160 * 8;

fn keys(source: &str, times: std::ops::Range<i64>) -> Vec<(String, i64)> {
    times.map(|time| (source.to_owned(), time)).collect()
}

#[test]
fn a_working_set_over_the_budget_is_refused_at_the_upload_that_outgrows_it() {
    // Seventeen 4K layers at the default 1 GiB: sixteen fit. Unpinned, the store would evict the
    // first to take the last, and the export would refill it, over and over, until it gave up.
    let mut store = FrameStore::sized(|bytes: &u64| *bytes, 1 << 30);
    store.pin(keys("video", 0..17));
    for time in 0..16 {
        store
            .insert(("video".into(), time), FRAME_4K)
            .expect("sixteen fit");
    }
    assert_eq!(
        store.insert(("video".into(), 16), FRAME_4K),
        Err(WorkingSetOverBudget {
            held: 16 * FRAME_4K,
            requested: FRAME_4K,
            budget: 1 << 30,
        }),
    );
    assert_eq!(
        store
            .insert(("video".into(), 16), FRAME_4K)
            .unwrap_err()
            .needed(),
        17 * FRAME_4K
    );
    // What it held stays: the sixteen it was given.
    assert!((0..16).all(|time| store.get("video", time).is_some()));
    assert!(store.get("video", 16).is_none());
    // Replacing a pinned frame counts the frames beside it, not the one it replaces.
    store
        .insert(("video".into(), 3), FRAME_4K)
        .expect("a replacement fits where its frame was");
    // So does aliasing an unpinned frame into the working set.
    store
        .insert(("other".into(), 0), FRAME_4K)
        .expect("an unpinned frame is kept as the newest");
    assert_eq!(
        store.alias(&("other".into(), 0), ("video".into(), 16)),
        Err(WorkingSetOverBudget {
            held: 16 * FRAME_4K,
            requested: FRAME_4K,
            budget: 1 << 30,
        }),
    );
    assert!(store.get("video", 16).is_none());
    // An alias of a frame the working set already holds adds nothing.
    assert_eq!(
        store.alias(&("video".into(), 0), ("video".into(), 16)),
        Ok(true)
    );
    // Unpinned, the budget rules alone again.
    store.unpin();
    store
        .insert(("video".into(), 17), FRAME_4K)
        .expect("unpinned uploads are never refused");
    assert!(store.retained_bytes(|bytes| *bytes) <= 1 << 30);
}

#[test]
fn a_working_set_of_one_frame_is_kept_over_the_budget() {
    // As any newest upload is: the frame must exist to be shown.
    let mut store = FrameStore::sized(|bytes: &u64| *bytes, 64);
    store.pin(keys("video", 0..1));
    store
        .insert(("video".into(), 0), 128)
        .expect("one frame is never refused");
    assert!(store.get("video", 0).is_some());
    // Its image counts as the frame where no exact frame answers.
    store.pin([("still".to_owned(), 0), ("video".to_owned(), 0)]);
    assert!(store.insert_image("still".into(), 1).is_err());
}

#[test]
fn the_budget_evicts_around_the_pinned_working_set() {
    // 256 MiB, a still and a 4K video on top of it, frame after frame: each frame's video
    // upload evicts the frames before, never the still the frame shows too.
    let mut store = FrameStore::sized(|bytes: &u64| *bytes, 256 << 20);
    let logo = store
        .insert_image("logo".into(), 8 << 20)
        .expect("admitted");
    for time in 0..8 {
        store.pin([("logo".to_owned(), time), ("video".to_owned(), time)]);
        store
            .insert(("video".into(), time), FRAME_4K)
            .expect("the frame's working set fits");
        assert!(store.holds_image("logo", logo), "the still at frame {time}");
        assert!(store.get("video", time).is_some());
        assert!(store.retained_bytes(|bytes| *bytes) <= 256 << 20);
    }
    // The count limit spares it too.
    for index in 0..MAX_IMAGES {
        store
            .insert_image(format!("photo-{index}"), 0)
            .expect("admitted");
    }
    assert!(store.holds_image("logo", logo));
    // An image a pinned key does not show (its exact frame answers) is not in the working set.
    store.pin([("video".to_owned(), 7)]);
    store
        .insert_image("video".into(), 1 << 30)
        .expect("not shown, so not refused");
}

#[test]
fn replacement_images_and_aliases_share_the_retained_byte_budget() {
    let mut store = FrameStore::sized(|bytes: &u64| *bytes, 64 << 20);
    for _ in 0..100 {
        store
            .insert(("video".into(), 0), 16 << 20)
            .expect("admitted");
        assert!(
            store
                .alias(&("video".into(), 0), ("video".into(), 1))
                .expect("admitted")
        );
        store
            .insert_image("image".into(), 16 << 20)
            .expect("admitted");
        assert!(store.retained_bytes(|bytes| *bytes) <= 64 << 20);
        assert!(store.get("video", 0).is_some());
        assert!(store.get("image", 0).is_some());
    }
    store.set_video_byte_budget(16 << 20);
    assert_eq!(store.retained_bytes(|bytes| *bytes), 16 << 20);
    store.set_video_byte_budget(0);
    assert_eq!(store.len(), 0);
}

#[test]
fn eviction_releases_cache_ownership_without_invalidating_an_owned_lease() {
    let mut store = FrameStore::sized(|_: &std::rc::Rc<()>| 1, 2);
    let lease = std::rc::Rc::new(());
    let weak = std::rc::Rc::downgrade(&lease);
    store
        .insert(("video".into(), 0), lease.clone())
        .expect("admitted");
    for _ in 0..10 {
        store
            .insert(("video".into(), 0), std::rc::Rc::new(()))
            .expect("admitted");
    }
    assert!(weak.upgrade().is_some());
    drop(lease);
    assert!(weak.upgrade().is_none());
}

#[test]
fn video_frames_stay_within_the_byte_budget_when_nothing_retires_them() {
    // A source that keeps missing means the worker never retires uploads, so the store alone bounds
    // them: 16 MB frames against a 64 MB budget leave the newest four.
    let mut store = FrameStore::sized(|bytes: &u64| *bytes, 64 << 20);
    for time in 0..100 {
        store
            .insert(("video".into(), time), 16 << 20)
            .expect("admitted");
    }
    assert!(store.video_bytes() <= 64 << 20);
    assert_eq!(store.video_bytes(), 64 << 20);
    for time in 96..100 {
        assert!(store.get("video", time).is_some(), "frame {time}");
    }
    assert!(store.get("video", 95).is_none());
    // One frame larger than the whole budget is still kept, because the frame being presented
    // must exist.
    store
        .insert(("video".into(), 100), 128 << 20)
        .expect("admitted");
    assert!(store.get("video", 100).is_some());
    assert_eq!(store.video_bytes(), 128 << 20);
    store.set_video_byte_budget(0);
    assert_eq!(store.video_bytes(), 0);
}

#[test]
fn retaining_sources_drops_only_the_others() {
    let mut store = FrameStore::default();
    store.insert(("kept".into(), 10), 1).expect("admitted");
    store
        .insert_image("kept-image".into(), 2)
        .expect("admitted");
    store.insert(("dropped".into(), 10), 3).expect("admitted");
    store
        .insert_image("dropped-image".into(), 4)
        .expect("admitted");
    store.retain_sources(|source| !source.starts_with("dropped"));
    assert_eq!(store.get("kept", 10), Some(&1));
    assert_eq!(store.get("kept-image", 0), Some(&2));
    assert_eq!(store.get("dropped", 10), None);
    assert_eq!(store.get("dropped-image", 0), None);
}

#[test]
fn an_image_the_store_dropped_on_its_own_is_no_longer_held() {
    let mut store = FrameStore::sized(|bytes: &u64| *bytes, 64);
    let logo = store.insert_image("logo".into(), 1).expect("admitted");
    assert!(store.holds_image("logo", logo));
    // The count limit drops the oldest image, which is the one a host
    // uploaded first and keeps showing.
    for index in 0..MAX_IMAGES {
        store
            .insert_image(format!("photo-{index}"), 0)
            .expect("admitted");
    }
    assert!(!store.holds_image("logo", logo));
    let logo = store.insert_image("logo".into(), 1).expect("admitted");
    assert!(store.holds_image("logo", logo));
    // So does the byte budget.
    store.insert(("video".into(), 0), 64).expect("admitted");
    assert!(!store.holds_image("logo", logo));
    // A replacement is a different upload.
    let first = store.insert_image("still".into(), 1).expect("admitted");
    let second = store.insert_image("still".into(), 1).expect("admitted");
    assert!(!store.holds_image("still", first));
    assert!(store.holds_image("still", second));
    assert!(!store.holds_image("missing", second));
}

#[test]
fn releasing_a_source_drops_its_frames_and_image_only() {
    let mut store = FrameStore::default();
    store.insert(("changed".into(), 0), 1).expect("admitted");
    store
        .alias(&("changed".into(), 0), ("changed".into(), 1))
        .expect("admitted");
    store.insert_image("changed".into(), 2).expect("admitted");
    store.insert(("kept".into(), 0), 3).expect("admitted");
    assert!(store.release_source("changed"));
    assert!(!store.release_source("changed"));
    assert_eq!(store.get("changed", 0), None);
    assert_eq!(store.get("changed", 1), None);
    assert_eq!(store.get("kept", 0), Some(&3));
}

#[test]
fn invalidating_derived_values_covers_retired_generations_and_aliases() {
    let mut store = FrameStore::default();
    store.insert_image("raw".into(), false).expect("admitted");
    store.insert(("video".into(), 0), true).expect("admitted");
    store
        .alias(&("video".into(), 0), ("video".into(), 1))
        .expect("admitted");
    store.insert(("video".into(), 0), true).expect("admitted");
    store.retain_values(|derived| !derived);
    assert_eq!(store.len(), 1);
    assert_eq!(store.retired_len(), 0);
    assert_eq!(store.get("raw", 0), Some(&false));
    assert_eq!(store.get("video", 0), None);
    assert_eq!(store.get("video", 1), None);
}

#[test]
fn video_lookup_remains_exact_but_images_cover_every_timestamp() {
    let mut store = FrameStore::default();
    store.insert(("video".into(), 42), 1).expect("admitted");
    let image = store.insert_image("image".into(), 2).expect("admitted");
    assert_eq!(store.get("video", 42), Some(&1));
    assert_eq!(store.get("video", 43), None);
    for time in [0, 1, 959_999, 960_000, i64::MAX] {
        assert_eq!(store.get("image", time), Some(&2));
    }
    assert!(store.release_generation(image));
    assert_eq!(store.get("image", 0), None);
}

#[test]
fn replacing_an_image_or_frame_does_not_release_the_new_generation() {
    let mut store = FrameStore::default();
    let old_image = store.insert_image("image".into(), 1).expect("admitted");
    let new_image = store.insert_image("image".into(), 2).expect("admitted");
    let old_video = store.insert(("video".into(), 10), 3).expect("admitted");
    let new_video = store.insert(("video".into(), 10), 4).expect("admitted");
    assert_eq!(store.len(), 4);
    assert!(store.release_generation(old_image));
    assert!(store.release_generation(old_video));
    assert!(!store.release_generation(old_video));
    assert_eq!(store.get("image", 1), Some(&2));
    assert_eq!(store.get("video", 10), Some(&4));
    assert!(store.release_generation(new_image));
    assert!(store.release_generation(new_video));
    assert_eq!(store.len(), 0);
}

#[test]
fn exact_requests_can_alias_one_physical_generation() {
    let mut store = FrameStore::default();
    let generation = store.insert(("video".into(), 10), 7_u64).expect("admitted");
    assert!(
        store
            .alias(&("video".into(), 10), ("video".into(), 11))
            .expect("admitted")
    );
    assert!(
        store
            .alias(&("video".into(), 11), ("video".into(), 10))
            .expect("admitted")
    );
    assert_eq!(store.get("video", 10), Some(&7));
    assert_eq!(store.get("video", 11), Some(&7));
    assert_eq!(store.len(), 1);
    assert_eq!(store.retired_len(), 0);
    assert_eq!(store.retained_bytes(|value| *value), 7);
    assert!(store.release_generation(generation));
    assert_eq!(store.get("video", 10), None);
    assert_eq!(store.get("video", 11), None);
    assert_eq!(store.len(), 0);
}

#[test]
fn aliasing_requires_a_live_exact_source_key() {
    let mut store = FrameStore::default();
    assert!(
        !store
            .alias(&("video".into(), 10), ("video".into(), 11))
            .expect("admitted")
    );
    let generation = store.insert(("video".into(), 10), 7_u64).expect("admitted");
    assert!(store.remove("video", 10));
    assert_eq!(store.get("video", 10), None);
    let generation = store
        .insert(("video".into(), 10), generation)
        .expect("admitted");
    assert!(store.release_generation(generation));
    assert!(
        !store
            .alias(&("video".into(), 10), ("video".into(), 11))
            .expect("admitted")
    );
}

#[test]
fn retained_generations_and_live_frames_are_bounded() {
    let mut store = FrameStore::default();
    for generation in 0..(MAX_RETIRED + 10) {
        store
            .insert(("same".into(), 1), generation)
            .expect("admitted");
    }
    assert_eq!(store.retired_len(), MAX_RETIRED);

    for time in 0..(MAX_VIDEO_FRAMES + 10) {
        store
            .insert(("video".into(), i64::try_from(time).expect("time")), time)
            .expect("admitted");
    }
    assert!(store.frames.len() <= MAX_VIDEO_FRAMES);
    store.insert(("other".into(), 0), 0).expect("admitted");
    store.trim_before("video", i64::try_from(MAX_VIDEO_FRAMES).expect("time"));
    assert!(store.frames.keys().all(|(source, time)| {
        source != "video" || *time >= i64::try_from(MAX_VIDEO_FRAMES).expect("time")
    }));
    // A time on one source's timeline trims only that source.
    assert!(store.frames.contains_key(&("other".into(), 0)));
    store.clear();
    assert_eq!(store.len(), 0);
}

#[test]
fn retained_bytes_include_live_images_frames_and_replaced_entries() {
    let mut store = FrameStore::default();
    store.insert(("video".into(), 1), 10_u64).expect("admitted");
    store.insert(("video".into(), 1), 20_u64).expect("admitted");
    store
        .insert_image("image".into(), 30_u64)
        .expect("admitted");
    assert_eq!(store.retained_bytes(|value| *value), 60);
}
