use nixcache_core::{
    CoreError, IndexEntry, NarDigest, NarInfo, NarInfoMeta, ShardDataPayload, ShardDescriptor,
    ShardedArchCacheIndexData, StoreHash, SystemArch,
};
use serde_json::{Value, json};
use std::collections::HashMap;

const NIX_HASH: &str = "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0";
const OCI_HEX: &str = "0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0";

fn store_hash() -> StoreHash {
    StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").unwrap()
}

fn meta(hash: &StoreHash, name: &str) -> NarInfoMeta {
    NarInfoMeta {
        store_path: format!("/nix/store/{hash}-{name}"),
        nar_basename: format!("{name}.nar.xz"),
        compression: Some("xz".to_string()),
        file_hash: Some(NIX_HASH.to_string()),
        file_size: Some(10),
        nar_hash: NIX_HASH.to_string(),
        references: Vec::new(),
        deriver: None,
        signatures: Vec::new(),
        ca: None,
    }
}

fn entry(hash: &StoreHash, name: &str) -> IndexEntry {
    IndexEntry {
        name: name.to_string(),
        system: Some(SystemArch::X86_64Linux),
        narinfo_meta: meta(hash, name),
        nar_digest: NarDigest::new_sha256(OCI_HEX).unwrap(),
        nar_size: 20,
        added: "2026-09-11T00:00:00Z".to_string(),
        origin: None,
    }
}

#[test]
fn nar_digest_accepts_only_canonical_sha256() {
    assert!(NarDigest::parse(&format!("sha256:{OCI_HEX}")).is_ok());
    assert_eq!(
        NarDigest::parse(&format!("sha256:{}", OCI_HEX.to_ascii_uppercase()))
            .unwrap()
            .as_str(),
        format!("sha256:{OCI_HEX}")
    );
    for value in [
        format!("md5:{OCI_HEX}"),
        format!("foo:{OCI_HEX}"),
        format!(":{OCI_HEX}"),
        OCI_HEX.to_string(),
        "sha256:1111".to_string(),
        "sha256:gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg".to_string(),
    ] {
        assert!(
            NarDigest::parse(&value).is_err(),
            "accepted invalid digest {value}"
        );
    }
    assert!(serde_json::from_str::<NarDigest>(&format!("\"md5:{OCI_HEX}\"")).is_err());
}

#[test]
fn narinfo_validates_fields_before_building_metadata() {
    let valid = format!(
        "StorePath: /nix/store/{}-hello\nURL: https://cache.example/nar/hello.nar.xz\n\
         Compression: xz\nFileHash: {NIX_HASH}\nFileSize: 10\nNarHash: {NIX_HASH}\n\
         NarSize: 20\nReferences: /nix/store/{}-dep cccccccccccccccccccccccccccccccc-dep\n\
         Deriver: /nix/store/{}-hello.drv\nSig: test:one\nSig: test:two\n",
        store_hash(),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    let parsed = NarInfo::parse(&valid).unwrap();
    assert_eq!(parsed.nar_basename(), "hello.nar.xz");
    assert_eq!(
        parsed.references,
        vec![
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep",
            "cccccccccccccccccccccccccccccccc-dep",
        ]
    );
    assert_eq!(
        parsed.deriver.as_deref(),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-hello.drv")
    );
    assert_eq!(NarInfo::parse(&parsed.to_narinfo_string()).unwrap(), parsed);

    for invalid in [
        valid.replace("StorePath: /nix/store/", "StorePath: /tmp/store/"),
        valid.replace("URL: https://cache.example/nar/hello.nar.xz", "URL: nar/../hello"),
        valid.replace(&format!("NarHash: {NIX_HASH}"), "NarHash: sha256:short"),
        valid.replace("References: /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep cccccccccccccccccccccccccccccccc-dep",
            "References: invalid/reference"),
        valid.replace("NarSize: 20", "NarSize: 0"),
        format!("{valid}StorePath: /nix/store/{}-duplicate\n", store_hash()),
    ] {
        assert!(NarInfo::parse(&invalid).is_err(), "accepted invalid narinfo:\n{invalid}");
    }
    assert!(matches!(
        NarInfo::parse(&format!("{valid}NarSize: 20\n")),
        Err(nixcache_core::NarInfoParseError::DuplicateField("NarSize"))
    ));
    assert!(matches!(
        NarInfo::parse("StorePath /nix/store/foo"),
        Err(nixcache_core::NarInfoParseError::MalformedLine(_))
    ));
}

#[test]
fn nested_metadata_deserialization_is_strict() {
    let hash = store_hash();
    let mut value = serde_json::to_value(meta(&hash, "hello")).unwrap();
    value["nar_hash"] = Value::String("sha256:short".to_string());
    assert!(serde_json::from_value::<NarInfoMeta>(value).is_err());

    let mut value = serde_json::to_value(entry(&hash, "hello")).unwrap();
    value["nar_size"] = json!(0);
    assert!(serde_json::from_value::<IndexEntry>(value).is_err());
}

#[test]
fn root_and_payload_deserialization_validate_structure() {
    let hash = store_hash();
    let mut root = ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "owner/repo", "ghcr.io");
    root.gc_roots.push(hash.clone());
    root.validate_structure().unwrap();

    let mut root_value = serde_json::to_value(&root).unwrap();
    root_value["version"] = json!(6);
    assert!(serde_json::from_value::<ShardedArchCacheIndexData>(root_value).is_err());

    let mut bad_root = serde_json::to_value(&root).unwrap();
    bad_root["system"] = json!("unknown");
    assert!(serde_json::from_value::<ShardedArchCacheIndexData>(bad_root).is_err());

    let payload = ShardDataPayload::with_entries(
        hash.shard_id(),
        HashMap::from([(hash.clone(), entry(&hash, "hello"))]),
    )
    .unwrap();
    payload.validate_structure().unwrap();
    let round_trip: ShardDataPayload =
        serde_json::from_value(serde_json::to_value(&payload).unwrap()).unwrap();
    assert_eq!(round_trip, payload);

    let mut bad_payload = serde_json::to_value(&payload).unwrap();
    bad_payload["entries"][hash.as_str()]["narinfo_meta"]["store_path"] =
        json!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-other");
    assert!(serde_json::from_value::<ShardDataPayload>(bad_payload).is_err());

    let mut bad_descriptor = ShardDescriptor::empty(0).unwrap();
    bad_descriptor.entry_count = 1;
    bad_descriptor.blob_digest = "sha256:bad".to_string();
    let mut descriptor_value = serde_json::to_value(&bad_descriptor).unwrap();
    descriptor_value["blob_digest"] =
        json!("md5:1111111111111111111111111111111111111111111111111111111111111111");
    assert!(serde_json::from_value::<ShardDescriptor>(descriptor_value).is_err());
}

#[test]
fn dependency_graph_does_not_silently_skip_invalid_references() {
    let hash = store_hash();
    let mut entries = HashMap::new();
    let mut item = entry(&hash, "hello");
    item.narinfo_meta
        .references
        .push("not-a-store-reference".to_string());
    entries.insert(hash, item);

    let selector = nixcache_core::CacheSelector::all(std::collections::HashSet::new());
    assert!(matches!(
        nixcache_core::evaluate_cache_query(&entries, &HashMap::new(), &selector),
        Err(CoreError::Type(_))
    ));
}
