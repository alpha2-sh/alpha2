use alpha2_media::{ColorMetadata, SourceId};
use alpha2_model::{
    AudioStreamShape, ColorPipeline, HdrSources, MediaFingerprint, Project, StreamTopology,
    VideoStreamShape,
};
use alpha2_time::FrameRate;
use serde_json::json;

use crate::{
    AssetCatalog, AssetEntry, AudioStreamFacts, ConformIssue, FingerprintField, FingerprintMatch,
    MediaPurpose, OriginalState, ProxyMismatch, ProxyProblem, ProxyState, ProxyVariant, Selection,
    color_pipeline_fingerprint, compare, conform, select,
};

fn rate(num: u32, den: u32) -> FrameRate {
    FrameRate::new(num, den).expect("rate")
}

fn hash(digit: char) -> String {
    std::iter::repeat_n(digit, 64).collect()
}

/// A 10 s 1080p 29.97 fps file with stereo 48 kHz audio.
fn fingerprint() -> MediaFingerprint {
    MediaFingerprint {
        content_sha256: Some(hash('a')),
        byte_length: Some(1_000_000),
        modified_ms: Some(1_790_000_000_000),
        streams: Some(StreamTopology {
            video: Some(VideoStreamShape::new(
                1_920,
                1_080,
                Some(rate(30_000, 1_001)),
            )),
            audio: Some(AudioStreamShape::new(2, 48_000)),
        }),
        duration_ticks: Some(1_200_000),
        color: Some(ColorMetadata::default()),
    }
}

fn unhashed() -> MediaFingerprint {
    MediaFingerprint {
        content_sha256: None,
        ..fingerprint()
    }
}

fn proxy(id: &str) -> ProxyVariant {
    ProxyVariant {
        id: id.into(),
        source_revision: 0,
        made_from: fingerprint(),
        width: 960,
        height: 540,
        frame_rate: Some(rate(30_000, 1_001)),
        duration_ticks: Some(1_200_000),
        audio_sample_rate: Some(48_000),
        color_pipeline: None,
        state: ProxyState::Ready,
    }
}

fn source_id(id: &str) -> SourceId {
    SourceId::new(id).expect("id")
}

fn entry(original: OriginalState, proxies: Vec<ProxyVariant>) -> AssetEntry {
    let mut entry = AssetEntry::new(source_id("video"), 0, original);
    entry.proxies = proxies;
    entry
}

fn online() -> OriginalState {
    OriginalState::Online {
        fingerprint: fingerprint(),
    }
}

/// A project showing `video` (recorded with [`fingerprint`]) and a generated fill, plus an
/// unused `spare` source.
fn project() -> Project {
    let recorded = serde_json::to_value(fingerprint()).expect("fingerprint");
    Project::from_json(
        &json!({
            "schema_version": 1,
            "id": "conform",
            "name": "Conform",
            "settings": {
                "width": 1920, "height": 1080,
                "frame_rate": { "num": 30000, "den": 1001 },
                "sample_rate": 48000
            },
            "sources": [
                { "id": "video", "locator": "asset://video", "kind": "video", "fingerprint": recorded },
                { "id": "spare", "locator": "asset://spare", "kind": "video" },
                { "id": "wash", "kind": "generated", "generator": {
                    "type": "fill",
                    "paint": { "type": "solid", "color": { "red": 1, "green": 0, "blue": 0, "alpha": 1 } }
                } }
            ],
            "timeline": { "tracks": [ {
                "id": "v1", "name": "V1", "kind": "video",
                "clips": [
                    { "id": "a", "source_id": "video",
                      "timeline_range": { "start": 0, "end": 120_000 },
                      "source_range": { "start": 0, "end": 120_000 } },
                    { "id": "b", "source_id": "wash",
                      "timeline_range": { "start": 120_000, "end": 240_000 },
                      "source_range": { "start": 0, "end": 120_000 } }
                ]
            } ] }
        })
        .to_string(),
    )
    .expect("project")
}

#[test]
fn hashes_decide_alone() {
    let copied = MediaFingerprint {
        modified_ms: Some(1),
        ..fingerprint()
    };
    assert_eq!(
        compare(&fingerprint(), &copied),
        FingerprintMatch::Identical
    );
    let edited = MediaFingerprint {
        content_sha256: Some(hash('b')),
        ..fingerprint()
    };
    assert_eq!(
        compare(&fingerprint(), &edited),
        FingerprintMatch::Mismatch {
            fields: vec![FingerprintField::Content]
        }
    );
}

#[test]
fn without_hashes_every_shared_part_is_compared_with_demuxer_tolerance() {
    // Another demuxer: an averaged rate, a duration one frame shorter, no color.
    let other = MediaFingerprint {
        streams: Some(StreamTopology {
            video: Some(VideoStreamShape::new(1_920, 1_080, Some(rate(2_997, 100)))),
            audio: Some(AudioStreamShape::new(2, 48_000)),
        }),
        duration_ticks: Some(1_200_000 - 4_004),
        color: None,
        ..unhashed()
    };
    assert_eq!(compare(&unhashed(), &other), FingerprintMatch::Consistent);
    let longer = MediaFingerprint {
        duration_ticks: Some(1_200_000 + 4_005),
        byte_length: Some(999),
        ..unhashed()
    };
    assert_eq!(
        compare(&unhashed(), &longer),
        FingerprintMatch::Mismatch {
            fields: vec![FingerprintField::ByteLength, FingerprintField::Duration]
        }
    );
    let silent = MediaFingerprint {
        streams: Some(StreamTopology {
            video: Some(VideoStreamShape::new(1_080, 1_920, None)),
            audio: None,
        }),
        ..unhashed()
    };
    assert_eq!(
        compare(&unhashed(), &silent),
        FingerprintMatch::Mismatch {
            fields: vec![FingerprintField::Streams, FingerprintField::Video]
        }
    );
    // Only a modification time in common, so nothing was compared.
    let dated = MediaFingerprint {
        modified_ms: Some(5),
        ..MediaFingerprint::default()
    };
    assert_eq!(compare(&unhashed(), &dated), FingerprintMatch::Unverified);
    assert!(FingerprintMatch::Unverified.is_acceptable());
}

#[test]
fn each_purpose_reads_its_media() {
    let pipeline = ColorPipeline::default();
    let both = entry(online(), vec![proxy("half")]);
    let original_only = entry(online(), Vec::new());
    let proxy_only = entry(OriginalState::Offline { path: None }, vec![proxy("half")]);
    let half = Selection::Proxy { id: "half".into() };
    let cases = [
        (
            MediaPurpose::Preview,
            [Selection::Original, Selection::Original, half.clone()],
        ),
        (
            MediaPurpose::PreviewProxy,
            [half.clone(), Selection::Original, half.clone()],
        ),
        (
            MediaPurpose::Thumbnail,
            [half.clone(), Selection::Original, half.clone()],
        ),
        (
            MediaPurpose::RemoteEdit,
            [half.clone(), Selection::Original, half.clone()],
        ),
        (
            MediaPurpose::ExportOriginal,
            [
                Selection::Original,
                Selection::Original,
                Selection::Unavailable,
            ],
        ),
        (
            MediaPurpose::ExportProxy,
            [half.clone(), Selection::Original, half],
        ),
    ];
    for (purpose, expected) in cases {
        let got = [&both, &original_only, &proxy_only]
            .map(|entry| select(entry, purpose, Some(&fingerprint()), &pipeline));
        assert_eq!(got, expected, "{purpose:?}");
    }
}

#[test]
fn a_proxy_stands_in_only_for_the_media_it_was_made_from() {
    let pipeline = ColorPipeline::default();
    let reference = fingerprint();
    let check = |proxy: ProxyVariant| {
        crate::proxy_problem(
            &entry(OriginalState::Offline { path: None }, Vec::new()),
            &proxy,
            Some(&reference),
            &pipeline,
        )
    };
    assert_eq!(check(proxy("fine")), None);
    assert_eq!(
        check(ProxyVariant {
            source_revision: 1,
            ..proxy("old")
        }),
        Some(ProxyProblem::StaleRevision)
    );
    let mut other = pipeline;
    other.hdr_peak_nits = 4_000;
    assert_eq!(
        check(ProxyVariant {
            color_pipeline: Some(color_pipeline_fingerprint(&other)),
            ..proxy("graded")
        }),
        Some(ProxyProblem::StaleColorPipeline)
    );
    assert_eq!(
        check(ProxyVariant {
            color_pipeline: Some(color_pipeline_fingerprint(&pipeline)),
            ..proxy("graded")
        }),
        None
    );
    assert_eq!(
        check(ProxyVariant {
            state: ProxyState::Generating,
            ..proxy("soon")
        }),
        Some(ProxyProblem::Pending)
    );
    assert_eq!(
        check(ProxyVariant {
            made_from: MediaFingerprint {
                content_sha256: Some(hash('c')),
                ..fingerprint()
            },
            frame_rate: Some(rate(25, 1)),
            duration_ticks: Some(600_000),
            width: 540,
            height: 960,
            audio_sample_rate: Some(44_100),
            ..proxy("wrong")
        }),
        Some(ProxyProblem::Invalid {
            reasons: vec![
                ProxyMismatch::MadeFrom,
                ProxyMismatch::FrameRate,
                ProxyMismatch::Duration,
                ProxyMismatch::Aspect,
                ProxyMismatch::AudioSampleRate,
            ]
        })
    );
    assert_eq!(
        check(ProxyVariant {
            audio_sample_rate: None,
            ..proxy("mute")
        }),
        Some(ProxyProblem::Invalid {
            reasons: vec![ProxyMismatch::AudioMissing]
        })
    );
}

fn catalog(entry: AssetEntry) -> AssetCatalog {
    AssetCatalog::new(vec![entry])
}

fn kinds(report: &crate::ConformReport) -> Vec<(String, bool)> {
    report.sources[0]
        .issues
        .iter()
        .map(|entry| {
            let value = serde_json::to_value(&entry.issue).expect("issue");
            (
                value["kind"].as_str().expect("kind").to_owned(),
                entry.blocking,
            )
        })
        .collect()
}

#[test]
fn conform_reports_what_blocks_each_purpose() {
    let project = project();
    // Nothing known: every purpose blocks. Unused and generated sources are not listed.
    let empty = conform(&project, &AssetCatalog::default(), MediaPurpose::Preview);
    assert!(!empty.ready);
    assert_eq!(empty.sources.len(), 1);
    assert_eq!(kinds(&empty), [("missing".into(), true)]);

    // Offline with a proxy: preview and proxy export go ahead, an original export does not.
    let offline = catalog(entry(
        OriginalState::Offline { path: None },
        vec![proxy("half")],
    ));
    let preview = conform(&project, &offline, MediaPurpose::Preview);
    assert!(preview.ready);
    assert_eq!(
        kinds(&preview),
        [("offline".into(), false), ("using_proxy".into(), false)]
    );
    let original = conform(&project, &offline, MediaPurpose::ExportOriginal);
    assert!(!original.ready);
    assert_eq!(original.sources[0].selection, Selection::Unavailable);
    assert_eq!(original.sources[0].usable_proxy.as_deref(), Some("half"));
    assert_eq!(kinds(&original), [("offline".into(), true)]);
    assert!(conform(&project, &offline, MediaPurpose::ExportProxy).ready);

    // Found media that differs: a preview warns, an export refuses.
    let relinked = catalog(entry(
        OriginalState::Online {
            fingerprint: MediaFingerprint {
                content_sha256: Some(hash('d')),
                ..fingerprint()
            },
        },
        Vec::new(),
    ));
    assert_eq!(
        kinds(&conform(&project, &relinked, MediaPurpose::Preview)),
        [("fingerprint_mismatch".into(), false)]
    );
    let export = conform(&project, &relinked, MediaPurpose::ExportOriginal);
    assert!(!export.ready);
    assert_eq!(
        export
            .blocking()
            .map(|(_, issue)| issue.clone())
            .collect::<Vec<_>>(),
        [ConformIssue::FingerprintMismatch {
            fields: vec![FingerprintField::Content]
        }]
    );

    // A stale proxy is reported, never read.
    let stale = catalog(entry(
        OriginalState::Offline { path: None },
        vec![ProxyVariant {
            source_revision: 7,
            ..proxy("old")
        }],
    ));
    let preview = conform(&project, &stale, MediaPurpose::Preview);
    assert!(!preview.ready);
    assert_eq!(
        kinds(&preview),
        [("offline".into(), true), ("proxy".into(), false)]
    );

    let fine = conform(
        &project,
        &catalog(entry(online(), Vec::new())),
        MediaPurpose::ExportOriginal,
    );
    assert!(fine.ready);
    assert_eq!(fine.sources[0].issues, [] as [conform::ConformEntry; 0]);
}

#[test]
fn an_hdr_source_blocks_a_project_that_refuses_hdr() {
    let mut project = project();
    project.settings.color.hdr = HdrSources::Reject;
    let hdr = catalog(entry(
        OriginalState::Online {
            fingerprint: MediaFingerprint {
                color: Some(ColorMetadata {
                    dynamic_range: alpha2_media::DynamicRange::Hdr,
                    transfer: alpha2_media::TransferFunction::Pq,
                    ..ColorMetadata::default()
                }),
                content_sha256: None,
                ..fingerprint()
            },
        },
        Vec::new(),
    ));
    let report = conform(&project, &hdr, MediaPurpose::Preview);
    assert!(!report.ready);
    assert_eq!(
        kinds(&report),
        [
            ("fingerprint_mismatch".into(), false),
            ("hdr_rejected".into(), true)
        ]
    );
}

#[test]
fn a_pq_or_hlg_source_is_hdr_whatever_its_dynamic_range_says() {
    // The renderer refuses a PQ or HLG signal as HDR, so conform does too.
    for transfer in [
        alpha2_media::TransferFunction::Pq,
        alpha2_media::TransferFunction::Hlg,
    ] {
        let color = ColorMetadata {
            dynamic_range: alpha2_media::DynamicRange::Sdr,
            transfer,
            ..ColorMetadata::default()
        };
        let mut project = project();
        project.settings.color.hdr = HdrSources::Reject;
        project.sources[0].color_metadata = Some(color);
        if let Some(recorded) = project.sources[0].fingerprint.as_mut() {
            recorded.color = Some(color);
        }
        let found = catalog(entry(
            OriginalState::Online {
                fingerprint: MediaFingerprint {
                    color: Some(color),
                    content_sha256: None,
                    ..fingerprint()
                },
            },
            Vec::new(),
        ));
        let report = conform(&project, &found, MediaPurpose::Preview);
        assert_eq!(
            kinds(&report),
            [("hdr_rejected".into(), true)],
            "{transfer:?}"
        );
    }
}

#[test]
fn a_video_that_declares_no_color_is_reported_as_read_by_the_fallback() {
    // Untagged video is read as BT.709 limited SDR and reported up front.
    let untagged = || {
        catalog(entry(
            OriginalState::Online {
                fingerprint: MediaFingerprint {
                    color: None,
                    content_sha256: None,
                    ..fingerprint()
                },
            },
            Vec::new(),
        ))
    };
    let mut project = project();
    if let Some(recorded) = project.sources[0].fingerprint.as_mut() {
        recorded.color = None;
    }
    for purpose in [MediaPurpose::Preview, MediaPurpose::ExportOriginal] {
        let report = conform(&project, &untagged(), purpose);
        assert!(report.ready, "{purpose:?}: the fallback blocks nothing");
        assert_eq!(
            kinds(&report),
            [("undeclared_color".into(), false)],
            "{purpose:?}"
        );
    }
    // A declaration in the project is what the source is read as.
    let mut declared = project.clone();
    declared.sources[0].color_metadata = Some(ColorMetadata::EXPORT);
    assert!(
        kinds(&conform(&declared, &untagged(), MediaPurpose::Preview)).is_empty(),
        "declared"
    );
    let tagged = catalog(entry(
        OriginalState::Online {
            fingerprint: MediaFingerprint {
                content_sha256: None,
                ..fingerprint()
            },
        },
        Vec::new(),
    ));
    let mut recorded = project.clone();
    recorded.sources[0].fingerprint = Some(fingerprint());
    assert!(
        kinds(&conform(&recorded, &tagged, MediaPurpose::Preview)).is_empty(),
        "tagged"
    );
    // Provider-drawn media is RGB, which the fallback matrix and range do not affect.
    let mut animated = project.clone();
    animated.sources[0].provider = Some(alpha2_model::ProviderSource::AnimatedImage);
    assert!(
        kinds(&conform(&animated, &untagged(), MediaPurpose::Preview)).is_empty(),
        "provider"
    );
    let offline = conform(
        &project,
        &catalog(entry(OriginalState::Offline { path: None }, Vec::new())),
        MediaPurpose::Preview,
    );
    assert_eq!(kinds(&offline), [("offline".into(), true)]);
}

#[test]
fn reports_and_catalogs_have_a_stable_wire_form() {
    let offline = catalog(entry(
        OriginalState::Offline { path: None },
        vec![proxy("half")],
    ));
    let wire = serde_json::to_value(&offline).expect("catalog");
    assert_eq!(wire["assets"][0]["original"], json!({ "state": "offline" }));
    assert_eq!(
        wire["assets"][0]["proxies"][0]["state"],
        json!({ "state": "ready" })
    );
    let round: AssetCatalog = serde_json::from_value(wire).expect("round trip");
    assert_eq!(round, offline);
    let report = conform(&project(), &offline, MediaPurpose::ExportOriginal);
    assert_eq!(
        serde_json::to_value(&report).expect("report"),
        json!({
            "purpose": "export_original",
            "ready": false,
            "sources": [{
                "source_id": "video",
                "selection": { "media": "unavailable" },
                "usable_proxy": "half",
                "issues": [{ "kind": "offline", "blocking": true }]
            }],
            "stale_data": []
        })
    );
}

#[test]
fn conform_checks_the_media_groups_and_composition_sources_read() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/used-sources-corpus.json");
    let cases: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("used-sources corpus"))
            .expect("JSON");
    for case in cases.as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        let project = Project::from_json(&case["project"].to_string()).expect("valid");
        let report = conform(
            &project,
            &AssetCatalog::default(),
            MediaPurpose::ExportOriginal,
        );
        let checked = report
            .sources
            .iter()
            .map(|source| source.source_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let read = case["used"]
            .as_array()
            .expect("used")
            .iter()
            .map(|id| id.as_str().expect("id"))
            .filter(|id| {
                project
                    .sources
                    .iter()
                    .any(|source| source.id.as_str() == *id && source.is_host_supplied())
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(checked, read, "{name}");
        assert!(!report.ready, "{name}");
    }
}

#[path = "../benches/support/sources.rs"]
mod sources;

#[test]
fn conform_takes_time_in_proportion_to_the_sources() {
    alpha2_scaling_support::assert_linear_shared(
        "conform",
        10_000 / alpha2_scaling_support::GROWTH,
        sources::many_sources,
        |(project, catalog)| {
            let report = conform(project, catalog, MediaPurpose::Preview);
            assert_eq!(report.sources.len(), project.sources.len());
        },
    );
}

#[test]
fn the_first_entry_for_a_source_is_the_one_conform_reads() {
    let twice = AssetCatalog::new(vec![
        entry(OriginalState::Offline { path: None }, Vec::new()),
        entry(online(), Vec::new()),
    ]);
    let report = conform(&project(), &twice, MediaPurpose::Preview);
    assert_eq!(kinds(&report), [("offline".into(), true)]);
}

#[test]
fn a_proxy_is_checked_against_the_recorded_media_when_the_found_original_differs() {
    let project = project();
    let other = MediaFingerprint {
        content_sha256: Some(hash('b')),
        ..fingerprint()
    };
    let relinked = |made_from: MediaFingerprint| {
        catalog(entry(
            OriginalState::Online {
                fingerprint: other.clone(),
            },
            vec![ProxyVariant {
                made_from,
                ..proxy("half")
            }],
        ))
    };
    // Made from the found media, not the recorded media: never read.
    let wrong = conform(
        &project,
        &relinked(other.clone()),
        MediaPurpose::ExportProxy,
    );
    assert!(!wrong.ready);
    assert_eq!(wrong.sources[0].selection, Selection::Original);
    assert_eq!(wrong.sources[0].usable_proxy, None);
    assert_eq!(
        kinds(&wrong),
        [
            ("fingerprint_mismatch".into(), true),
            ("proxy".into(), false)
        ]
    );
    // Made from the recorded media: a proxy export reads it.
    let right = conform(
        &project,
        &relinked(fingerprint()),
        MediaPurpose::ExportProxy,
    );
    assert!(right.ready, "{:?}", kinds(&right));
    assert_eq!(
        right.sources[0].selection,
        Selection::Proxy { id: "half".into() }
    );
    let original = conform(
        &project,
        &relinked(fingerprint()),
        MediaPurpose::ExportOriginal,
    );
    assert!(!original.ready);
    assert_eq!(original.sources[0].usable_proxy.as_deref(), Some("half"));
}

#[test]
fn a_recorded_hdr_original_blocks_a_project_that_refuses_hdr_while_it_is_offline() {
    let mut project = project();
    project.settings.color.hdr = HdrSources::Reject;
    if let Some(recorded) = project.sources[0].fingerprint.as_mut() {
        recorded.color = Some(ColorMetadata {
            dynamic_range: alpha2_media::DynamicRange::Hdr,
            transfer: alpha2_media::TransferFunction::Pq,
            ..ColorMetadata::default()
        });
    }
    let offline = catalog(entry(
        OriginalState::Offline { path: None },
        vec![proxy("half")],
    ));
    for purpose in [MediaPurpose::Preview, MediaPurpose::ExportProxy] {
        let report = conform(&project, &offline, purpose);
        assert!(!report.ready, "{purpose:?}");
        assert_eq!(
            kinds(&report),
            [
                ("offline".into(), false),
                ("using_proxy".into(), false),
                ("hdr_rejected".into(), true)
            ],
            "{purpose:?}"
        );
    }
}

#[test]
fn a_rate_only_one_side_declares_sets_the_duration_tolerance_either_way() {
    let shape = |rate| {
        Some(StreamTopology {
            video: Some(VideoStreamShape::new(1_920, 1_080, rate)),
            audio: None,
        })
    };
    let without = MediaFingerprint {
        streams: shape(None),
        duration_ticks: Some(1_200_000),
        ..MediaFingerprint::default()
    };
    let with = MediaFingerprint {
        streams: shape(Some(rate(30_000, 1_001))),
        duration_ticks: Some(1_204_000),
        ..MediaFingerprint::default()
    };
    assert_eq!(compare(&without, &with), FingerprintMatch::Consistent);
    assert_eq!(compare(&with, &without), FingerprintMatch::Consistent);
    // A proxy's declared rate does the same for a recording without one.
    let reference = MediaFingerprint {
        streams: shape(None),
        duration_ticks: Some(1_200_000),
        ..MediaFingerprint::default()
    };
    let longer = ProxyVariant {
        made_from: MediaFingerprint::default(),
        duration_ticks: Some(1_204_000),
        audio_sample_rate: None,
        ..proxy("half")
    };
    assert_eq!(
        crate::proxy_problem(
            &entry(OriginalState::Offline { path: None }, Vec::new()),
            &longer,
            Some(&reference),
            &ColorPipeline::default(),
        ),
        None
    );
}

/// `tests/cases/media-evidence-corpus.json` holds every host to the same table.
#[test]
fn a_measured_rate_rounds_as_every_host_rounds_it() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/media-evidence-corpus.json");
    let corpus: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("corpus")).expect("JSON");
    for case in corpus["rational"].as_array().expect("table") {
        let measured = case[0].as_f64().expect("measured");
        let expected = case[1].as_object().map(|listed| {
            let part = |key: &str| u32::try_from(listed[key].as_u64().expect(key)).expect("u32");
            rate(part("num"), part("den"))
        });
        assert_eq!(crate::rational_rate(measured), expected, "{measured}");
    }
}

#[test]
fn a_frame_rate_is_measured_from_presentation_times() {
    let measure = |times: &[f64]| {
        let mut evidence = crate::FrameRateEvidence::new();
        let taken = times
            .iter()
            .take_while(|&&time| evidence.push(time))
            .count();
        (taken, evidence.rate())
    };
    // Ten frames at 10 fps, then ten at 5 fps: 19 frames over 2.8 s.
    let variable = (0..10)
        .map(|frame| f64::from(frame) / 10.0)
        .chain((0..10).map(|frame| 1.0 + f64::from(frame) / 5.0))
        .collect::<Vec<_>>();
    assert_eq!(measure(&variable), (20, Some(rate(6_786, 1_000))));
    // Stored with B-frames (I P B B ...): the same rate as in order.
    let reordered = (0..200)
        .map(|frame| {
            let frame = match frame % 3 {
                0 => frame,
                1 => frame + 1,
                _ => frame - 1,
            };
            f64::from(frame) * 1_001.0 / 30_000.0
        })
        .collect::<Vec<_>>();
    let (taken, measured) = measure(&reordered);
    assert_eq!(measured, Some(rate(30_000, 1_001)));
    // After 120 frames, the 121st stored is presented after all of them, so the measure stops.
    assert_eq!(taken, 120);
    assert_eq!(measure(&[0.0]).1, None);
    assert_eq!(measure(&[1.0, 1.0, 1.0]).1, None);
    assert_eq!(measure(&[f64::NAN, 0.0, 0.5]).1, Some(rate(2, 1)));
}

#[test]
fn the_tolerances_between_hosts_hold_at_their_limits() {
    use crate::rates_agree;
    assert!(rates_agree(rate(2_997, 100), rate(30_000, 1_001)));
    assert!(rates_agree(rate(30, 1), rate(29_986, 1_000)), "0.047 %");
    assert!(!rates_agree(rate(30, 1), rate(29_984, 1_000)), "0.053 %");
    assert!(!rates_agree(rate(30, 1), rate(29_940, 1_000)), "0.2 %");
    for whole in [24, 30, 48, 60, 120] {
        assert!(
            !rates_agree(rate(whole, 1), rate(whole * 1_000, 1_001)),
            "{whole} against its NTSC neighbour"
        );
    }
    // A 30p proxy of a 29.97 original does not map its timestamps one to one.
    let pipeline = ColorPipeline::default();
    let thirty = ProxyVariant {
        frame_rate: Some(rate(30, 1)),
        ..proxy("30p")
    };
    assert_eq!(
        crate::proxy_problem(
            &entry(OriginalState::Offline { path: None }, Vec::new()),
            &thirty,
            Some(&fingerprint()),
            &pipeline,
        ),
        Some(ProxyProblem::Invalid {
            reasons: vec![ProxyMismatch::FrameRate]
        })
    );

    let duration = |streams: Option<StreamTopology>, ticks: i64| {
        let recorded = MediaFingerprint {
            streams: streams.clone(),
            duration_ticks: Some(1_200_000),
            ..MediaFingerprint::default()
        };
        let found = MediaFingerprint {
            streams,
            duration_ticks: Some(1_200_000 + ticks),
            ..MediaFingerprint::default()
        };
        compare(&recorded, &found).is_acceptable()
    };
    let audio = Some(StreamTopology {
        video: None,
        audio: Some(AudioStreamShape::new(2, 48_000)),
    });
    // 1024 samples at 48 kHz: 2560 ticks.
    assert!(duration(audio.clone(), 2_560));
    assert!(!duration(audio, 2_561));
    // Nothing that says a frame: a millisecond, 120 ticks.
    assert!(duration(None, -120));
    assert!(!duration(None, -121));
}

/// A failed or unready proxy is never read, even with no evidence to check it against.
#[test]
fn a_failed_proxy_is_reported_and_never_read() {
    let failed = ProxyVariant {
        width: 0,
        height: 0,
        frame_rate: None,
        duration_ticks: None,
        audio_sample_rate: None,
        made_from: MediaFingerprint::default(),
        state: ProxyState::Failed {
            reason: "the proxy file is missing".into(),
        },
        ..proxy("gone")
    };
    let offline = entry(OriginalState::Offline { path: None }, vec![failed.clone()]);
    let pipeline = ColorPipeline::default();
    let problem = Some(ProxyProblem::Failed {
        reason: "the proxy file is missing".into(),
    });
    for reference in [None, Some(MediaFingerprint::default())] {
        assert_eq!(
            crate::proxy_problem(&offline, &failed, reference.as_ref(), &pipeline),
            problem
        );
        assert_eq!(
            select(
                &offline,
                MediaPurpose::ExportProxy,
                reference.as_ref(),
                &pipeline
            ),
            Selection::Unavailable
        );
    }
    let mut unrecorded = project();
    unrecorded.sources[0].fingerprint = None;
    let report = conform(&unrecorded, &catalog(offline), MediaPurpose::ExportProxy);
    assert!(!report.ready);
    assert_eq!(
        serde_json::to_value(&report.sources[0].issues).expect("issues"),
        json!([
            { "kind": "offline", "blocking": true },
            { "kind": "proxy", "proxy": "gone",
              "problem": { "kind": "failed", "reason": "the proxy file is missing" },
              "blocking": false }
        ])
    );
}

#[test]
fn a_recording_that_shares_nothing_with_the_media_found_is_unverified() {
    let mut project = project();
    project.sources[0].fingerprint = Some(MediaFingerprint {
        modified_ms: Some(5),
        ..MediaFingerprint::default()
    });
    for purpose in [MediaPurpose::Preview, MediaPurpose::ExportOriginal] {
        let report = conform(&project, &catalog(entry(online(), Vec::new())), purpose);
        assert!(report.ready, "{purpose:?}");
        assert_eq!(
            kinds(&report),
            [("fingerprint_unverified".into(), false)],
            "{purpose:?}"
        );
    }
}

#[test]
fn a_catalog_with_malformed_evidence_is_refused_where_it_is_malformed() {
    let upper = MediaFingerprint {
        content_sha256: Some("A".repeat(64)),
        ..fingerprint()
    };
    let found = catalog(entry(
        OriginalState::Online {
            fingerprint: upper.clone(),
        },
        Vec::new(),
    ));
    let error = found.validate().expect_err("an uppercase hash");
    assert_eq!(
        error.to_string(),
        "source video: the original's fingerprint has a malformed content_sha256"
    );
    let negative = catalog(entry(
        OriginalState::Offline { path: None },
        vec![ProxyVariant {
            made_from: MediaFingerprint {
                duration_ticks: Some(-5),
                ..MediaFingerprint::default()
            },
            ..proxy("half")
        }],
    ));
    assert_eq!(
        negative
            .validate()
            .expect_err("a negative duration")
            .to_string(),
        "source video: proxy half's made_from has a malformed duration_ticks"
    );
    assert_eq!(
        catalog(entry(online(), vec![proxy("half")])).validate(),
        Ok(())
    );
    assert_eq!(upper.malformed_part(), Some("content_sha256"));
    assert_eq!(fingerprint().malformed_part(), None);
}

#[test]
fn a_clip_choosing_an_audio_stream_or_channel_the_media_lacks_blocks() {
    let with = |stream: Option<u32>, channels: Option<Vec<u32>>| {
        let mut project = project();
        let audio = &mut project.timeline.tracks[0].clips[0].audio;
        audio.stream = stream;
        audio.channels = channels;
        project
    };
    let mut listed = entry(online(), Vec::new());
    listed.audio_streams = Some(vec![
        AudioStreamFacts::new(2, false),
        AudioStreamFacts::new(4, true),
    ]);
    let check = |project: &Project, entry: &AssetEntry| {
        let report = conform(
            project,
            &catalog(entry.clone()),
            MediaPurpose::ExportOriginal,
        );
        report.sources[0]
            .issues
            .iter()
            .filter_map(|entry| match &entry.issue {
                ConformIssue::AudioSelection { clip, reason } => {
                    assert!(entry.blocking);
                    Some(format!("{clip}: {reason}"))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    // The default stream has four channels; stream 0 has two.
    let none: Vec<String> = Vec::new();
    assert_eq!(check(&with(None, Some(vec![3, 2])), &listed), none);
    assert_eq!(check(&with(Some(1), Some(vec![0])), &listed), none);
    assert_eq!(
        check(&with(Some(0), Some(vec![2])), &listed),
        ["a: channel 2 does not exist; the audio stream has 2 channels"]
    );
    assert_eq!(
        check(&with(Some(2), None), &listed),
        ["a: audio stream 2 does not exist; the media has 2 audio streams"]
    );
    // Unlisted streams are not checked.
    assert_eq!(
        check(&with(Some(5), Some(vec![9])), &entry(online(), Vec::new())),
        none
    );
    let wire = serde_json::to_value(&listed).expect("entry");
    assert_eq!(
        wire["audio_streams"][1],
        json!({ "channels": 4, "default": true })
    );
}

#[test]
fn data_made_from_other_media_is_reported_stale() {
    let mut document = serde_json::to_value(project()).expect("json");
    let other = MediaFingerprint {
        content_sha256: Some(hash('b')),
        ..fingerprint()
    };
    document["data"] = json!([
        { "id": "made-here", "anchor": { "type": "source", "source_id": "video" }, "kind": "intervals",
          "intervals": [], "made_from": fingerprint() },
        { "id": "made-elsewhere", "anchor": { "type": "source", "source_id": "video" }, "kind": "intervals",
          "intervals": [], "made_from": other },
        { "id": "unrecorded", "anchor": { "type": "source", "source_id": "video" }, "kind": "intervals",
          "intervals": [] },
        { "id": "timeline", "anchor": { "type": "timeline" }, "kind": "intervals", "intervals": [] }
    ]);
    let project = Project::from_json(&document.to_string()).expect("project");
    let online = catalog(entry(
        OriginalState::Online {
            fingerprint: fingerprint(),
        },
        Vec::new(),
    ));
    let report = conform(&project, &online, MediaPurpose::Preview);
    assert!(report.ready, "stale data never blocks");
    assert_eq!(
        report.stale_data,
        vec![crate::StaleData {
            data: alpha2_model::DataSetId::new("made-elsewhere").expect("id"),
            source: SourceId::new("video").expect("id"),
            fields: vec![FingerprintField::Content],
        }]
    );
    // Media that cannot be read is not judged.
    let offline = catalog(entry(OriginalState::Offline { path: None }, Vec::new()));
    assert_eq!(
        conform(&project, &offline, MediaPurpose::Preview).stale_data,
        Vec::new()
    );
}
