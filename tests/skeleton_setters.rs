//! Typed message-header / UTC **writer** round-trips for the Skeleton
//! metadata bitstream.
//!
//! The Skeleton module already carries a typed *reader* for every
//! `docs/container/ogg/ogg-skeleton-message-headers.wiki` field
//! (`content_type()`, `role()`, `languages()`, `altitude()`,
//! `display_hint()`, `title()`, `name()`) and for the `fishead` UTC slot
//! (`utc_time()`). These tests pin the write-side symmetry added this
//! round: `FisBone::set_content_type` / `set_role` / `set_display_hint` /
//! `set_languages` / `set_altitude` / `set_title` / `set_name` and
//! `FisHead::set_utc` / `set_utc_str`, each asserted as the exact inverse
//! of its reader.

use oxideav_ogg::skeleton::{
    ContentType, ContentTypeKind, DisplayCoord, DisplayHint, FisBone, FisHead, Rational, Role,
    RoleKind, Utc, Version,
};

fn bone() -> FisBone {
    FisBone::new(0x1234_5678, Rational::new(48_000, 1))
}

#[test]
fn set_content_type_round_trips_through_reader() {
    let mut b = bone();
    let ct = ContentType::parse("audio/vorbis").unwrap();
    b.set_content_type(&ct);
    assert_eq!(b.header("Content-Type"), Some("audio/vorbis"));
    let got = b.content_type().unwrap().unwrap();
    assert_eq!(got, ct);
    assert!(matches!(got.kind, ContentTypeKind::Audio));
}

#[test]
fn set_content_type_preserves_mime_parameters() {
    let mut b = bone();
    let ct = ContentType::parse("audio/ogg;codecs=opus").unwrap();
    b.set_content_type(&ct);
    // The parameter survives the writer.
    assert_eq!(b.header("Content-Type"), Some("audio/ogg;codecs=opus"));
    let got = b.content_type().unwrap().unwrap();
    assert_eq!(got.parameter("codecs"), Some("opus"));
    assert_eq!(got, ct);
}

#[test]
fn set_content_type_other_kind_keeps_casing() {
    let mut b = bone();
    // application/kate — wiki §Role note: "application/kate is semantically
    // a text format". The Other kind preserves the original token.
    let ct = ContentType::parse("application/kate").unwrap();
    b.set_content_type(&ct);
    assert_eq!(b.header("Content-Type"), Some("application/kate"));
    assert_eq!(b.content_type().unwrap().unwrap(), ct);
}

#[test]
fn set_role_round_trips_with_parameters() {
    let mut b = bone();
    // The wiki §Role worked example with a parameter.
    let role = Role::parse("video/alternate;angle=nw");
    b.set_role(&role);
    assert_eq!(b.header("Role"), Some("video/alternate;angle=nw"));
    let got = b.role().unwrap();
    assert_eq!(got, role);
    assert!(matches!(got.kind, RoleKind::VideoAlternate));
    assert_eq!(got.parameter("angle"), Some("nw"));
}

#[test]
fn set_role_bare_tag_round_trips() {
    let mut b = bone();
    let role = Role::parse("audio/main");
    b.set_role(&role);
    assert_eq!(b.header("Role"), Some("audio/main"));
    assert_eq!(b.role().unwrap(), role);
}

#[test]
fn set_display_hint_pip_two_arg_round_trips() {
    let mut b = bone();
    let hint = DisplayHint::parse("pip(20%,20%)").unwrap();
    b.set_display_hint(&hint);
    assert_eq!(b.header("Display-hint"), Some("pip(20%,20%)"));
    let got = b.display_hint().unwrap().unwrap();
    assert_eq!(got, hint);
    match got {
        DisplayHint::Pip {
            x,
            y,
            width,
            height,
        } => {
            assert_eq!(x, DisplayCoord::Percent(20.0));
            assert_eq!(y, DisplayCoord::Percent(20.0));
            assert!(width.is_none() && height.is_none());
        }
        _ => panic!("expected Pip"),
    }
}

#[test]
fn set_display_hint_pip_four_arg_pixels_round_trips() {
    let mut b = bone();
    let hint = DisplayHint::parse("pip(40,40,690,60)").unwrap();
    b.set_display_hint(&hint);
    assert_eq!(b.header("Display-hint"), Some("pip(40,40,690,60)"));
    assert_eq!(b.display_hint().unwrap().unwrap(), hint);
}

#[test]
fn set_display_hint_mask_variants_round_trip() {
    for raw in [
        "mask(http://www.example.com/image.png)",
        "mask(http://www.example.com/image.png,30%,25%)",
        "mask(http://www.example.com/image.png,20,20,400,320)",
    ] {
        let mut b = bone();
        let hint = DisplayHint::parse(raw).unwrap();
        b.set_display_hint(&hint);
        assert_eq!(b.header("Display-hint"), Some(raw));
        assert_eq!(b.display_hint().unwrap().unwrap(), hint);
    }
}

#[test]
fn set_display_hint_transparent_round_trips() {
    let mut b = bone();
    let hint = DisplayHint::parse("transparent(25%)").unwrap();
    b.set_display_hint(&hint);
    assert_eq!(b.header("Display-hint"), Some("transparent(25%)"));
    assert_eq!(b.display_hint().unwrap().unwrap(), hint);
}

#[test]
fn set_display_hint_other_round_trips() {
    let mut b = bone();
    let hint = DisplayHint::parse("rotate(90,clockwise)").unwrap();
    b.set_display_hint(&hint);
    assert_eq!(b.header("Display-hint"), Some("rotate(90,clockwise)"));
    assert_eq!(b.display_hint().unwrap().unwrap(), hint);
}

#[test]
fn set_languages_round_trips_through_reader() {
    let mut b = bone();
    b.set_languages(&["en-US", "fr"]);
    // The wiki §Language worked example shape.
    assert_eq!(b.header("Language"), Some("en-US, fr"));
    assert_eq!(b.languages().unwrap(), vec!["en-US", "fr"]);
    assert_eq!(b.dominant_language(), Some("en-US"));
}

#[test]
fn set_languages_drops_blank_fragments() {
    let mut b = bone();
    b.set_languages(&["  de-DE ", "", "  "]);
    assert_eq!(b.header("Language"), Some("de-DE"));
    assert_eq!(b.languages().unwrap(), vec!["de-DE"]);
}

#[test]
fn set_languages_empty_removes_header() {
    let mut b = bone();
    b.set_languages(&["en"]);
    assert!(b.header("Language").is_some());
    b.set_languages::<&str>(&[]);
    assert!(b.header("Language").is_none());
    assert!(b.languages().is_none());
}

#[test]
fn set_altitude_round_trips_negative_value() {
    let mut b = bone();
    // The wiki §Altitude worked example.
    b.set_altitude(-150);
    assert_eq!(b.header("Altitude"), Some("-150"));
    assert_eq!(b.altitude().unwrap().unwrap(), -150);
}

#[test]
fn set_title_stores_value_verbatim() {
    let mut b = bone();
    b.set_title("the French audio track for the movie");
    let got = b.title().unwrap();
    assert_eq!(got.display(), "the French audio track for the movie");
}

#[test]
fn set_name_round_trips_well_formed() {
    let mut b = bone();
    b.set_name("Madonna_singing");
    let got = b.name().unwrap();
    assert_eq!(got.raw(), "Madonna_singing");
    assert!(got.is_well_formed());
}

#[test]
fn remove_header_reports_presence() {
    let mut b = bone();
    b.set_header("Role", "audio/main");
    assert!(b.remove_header("ROLE")); // case-insensitive
    assert!(!b.remove_header("Role"));
    assert!(b.role().is_none());
}

#[test]
fn set_utc_round_trips_through_reader() {
    let mut head = FisHead::new(Version::V4_0);
    let utc = Utc::parse("20260628T143000.500Z").unwrap();
    assert!(head.set_utc(&utc));
    assert_eq!(head.utc_str().as_deref(), Some("20260628T143000.500Z"));
    let got = head.utc_time().unwrap().unwrap();
    assert_eq!(got, utc);
    // Fractional zeros are preserved.
    assert_eq!(got.fraction, "500");
}

#[test]
fn set_utc_no_fraction_round_trips() {
    let mut head = FisHead::new(Version::V3_0);
    let utc = Utc::parse("20260628T143000Z").unwrap();
    assert!(head.set_utc(&utc));
    assert_eq!(head.utc_str().as_deref(), Some("20260628T143000Z"));
    assert_eq!(head.utc_time().unwrap().unwrap(), utc);
}

#[test]
fn set_utc_str_rejects_overlong_slot() {
    let mut head = FisHead::new(Version::V4_0);
    // 21 bytes — one past the fixed 20-byte slot.
    assert!(!head.set_utc_str("123456789012345678901"));
    // Slot stays empty (untouched).
    assert!(head.utc_str().is_none());
}

#[test]
fn fishead_bytes_round_trip_after_set_utc() {
    // The UTC slot written by set_utc survives a full to_bytes/parse cycle.
    let mut head = FisHead::new(Version::V4_0);
    head.set_utc(&Utc::parse("20010203T040506.7Z").unwrap());
    let bytes = head.to_bytes();
    let reparsed = FisHead::parse(&bytes).unwrap();
    assert_eq!(reparsed.utc_str().as_deref(), Some("20010203T040506.7Z"));
}

#[test]
fn fisbone_bytes_round_trip_after_typed_setters() {
    // Every typed setter survives the full fisbone to_bytes/parse cycle.
    let mut b = bone();
    b.set_content_type(&ContentType::parse("video/theora").unwrap());
    b.set_role(&Role::parse("video/main"));
    b.set_name("main_video");
    b.set_languages(&["en"]);
    b.set_altitude(-5);
    b.set_display_hint(&DisplayHint::parse("transparent(10%)").unwrap());
    let bytes = b.to_bytes();
    let reparsed = FisBone::parse(&bytes).unwrap();
    assert_eq!(
        reparsed.content_type().unwrap().unwrap(),
        ContentType::parse("video/theora").unwrap()
    );
    assert!(matches!(reparsed.role().unwrap().kind, RoleKind::VideoMain));
    assert_eq!(reparsed.name().unwrap().raw(), "main_video");
    assert_eq!(reparsed.languages().unwrap(), vec!["en"]);
    assert_eq!(reparsed.altitude().unwrap().unwrap(), -5);
    assert_eq!(
        reparsed.display_hint().unwrap().unwrap(),
        DisplayHint::parse("transparent(10%)").unwrap()
    );
}
