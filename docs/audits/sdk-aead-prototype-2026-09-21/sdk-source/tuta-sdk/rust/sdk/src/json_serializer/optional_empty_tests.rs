use super::*;
use crate::bindings::{file_client::MockFileClient, rest_client::MockRestClient};
use crate::entities::{generated::tutanota::Body, Entity};

#[test]
fn optional_encrypted_empty_fields_map_to_null() {
	let provider = Arc::new(TypeModelProvider::new_test(
		Arc::new(MockRestClient::new()),
		Arc::new(MockFileClient::new()),
		"http://test".to_owned(),
	));
	let serializer = JsonSerializer::new(provider);
	for field in ["1275", "1276"] {
		let mut raw: RawEntity =
			serde_json::from_str(r#"{"1274":"body","1275":null,"1276":null}"#).unwrap();
		assert_eq!(
			serializer.parse(&Body::type_ref(), raw.clone()).unwrap()[field],
			ElementValue::Null
		);
		raw.insert(field.to_owned(), JsonElement::String(String::new()));
		assert_eq!(
			serializer.parse(&Body::type_ref(), raw.clone()).unwrap()[field],
			ElementValue::Null
		);
		raw.insert(
			field.to_owned(),
			JsonElement::String(BASE64_STANDARD.encode([1; 16])),
		);
		assert_eq!(
			serializer.parse(&Body::type_ref(), raw.clone()).unwrap()[field],
			ElementValue::Bytes(vec![1; 16])
		);
		raw.insert(
			field.to_owned(),
			JsonElement::String("invalid-base64!".to_owned()),
		);
		assert!(serializer.parse(&Body::type_ref(), raw).is_err());
	}
}
