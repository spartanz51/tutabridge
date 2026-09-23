use super::EntityClient;
use crate::element_value::ParsedEntity;
use crate::json_element::RawEntity;
use crate::metamodel::ElementType;
use crate::{ApiCallError, GeneratedId, TypeRef};

impl EntityClient {
	/// Loads list elements in batches of at most 100 IDs.
	/// The server may omit missing entities and return them in a different order.
	pub async fn load_multiple(
		&self,
		type_ref: &TypeRef,
		list_id: &GeneratedId,
		element_ids: &[GeneratedId],
	) -> Result<Vec<ParsedEntity>, ApiCallError> {
		if element_ids.is_empty() {
			return Ok(Vec::new());
		}
		let model = self.resolve_client_type_ref(type_ref)?;
		if model.element_type != ElementType::ListElement {
			return Err(ApiCallError::internal(
				"load_multiple requires a list element type".to_owned(),
			));
		}
		let mut entities = Vec::new();
		for chunk in element_ids.chunks(100) {
			let ids = chunk
				.iter()
				.map(GeneratedId::as_str)
				.collect::<Vec<_>>()
				.join(",");
			let query = crate::bindings::rest_client::encode_query_params([("ids", ids)]);
			let url = format!(
				"{}/rest/{}/{}/{}{query}",
				self.base_url, type_ref.app, model.name, list_id
			);
			let body = self.prepare_and_fire(type_ref, url).await?.ok_or_else(|| {
				ApiCallError::internal("Missing load_multiple response body".to_owned())
			})?;
			let raw: Vec<RawEntity> = serde_json::from_slice(&body).map_err(|e| {
				ApiCallError::internal_with_err(e, "Invalid load_multiple response")
			})?;
			for entity in raw {
				entities.push(self.json_serializer.parse(type_ref, entity)?);
			}
		}
		Ok(entities)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::bindings::rest_client::{MockRestClient, RestResponse};
	use crate::element_value::ElementValue;
	use crate::json_serializer::JsonSerializer;
	use crate::metamodel::{AppName, TypeId};
	use crate::util::test_utils::{mock_type_model_provider, server_types_hash_header};
	use crate::{HeadersProvider, IdTupleGenerated};
	use std::sync::{Arc, Mutex};
	fn type_ref() -> TypeRef {
		TypeRef {
			app: AppName::EntityClientTestApp,
			type_id: TypeId::from(10),
		}
	}
	fn client(rest: MockRestClient) -> EntityClient {
		let model = Arc::new(mock_type_model_provider());
		EntityClient::new(
			Arc::new(rest),
			Arc::new(JsonSerializer::new(model.clone())),
			"http://test.com".into(),
			Arc::new(HeadersProvider::new(None)),
			model,
		)
	}
	fn response(status: u32, body: Option<Vec<u8>>) -> RestResponse {
		RestResponse {
			status,
			headers: server_types_hash_header(),
			body,
		}
	}
	#[tokio::test]
	async fn empty_ids_do_not_request() {
		let mut rest = MockRestClient::new();
		rest.expect_request_binary().times(0);
		assert!(client(rest)
			.load_multiple(&type_ref(), &GeneratedId("list".into()), &[])
			.await
			.unwrap()
			.is_empty());
	}
	#[tokio::test]
	async fn accepts_missing_ids_and_preserves_server_order() {
		let mut rest = MockRestClient::new();
		rest.expect_request_binary().times(1).returning(|_,_,_|Ok(response(200,Some(br#"[{"101":["list","second"],"102":"AQID"},{"101":["list","first"],"102":"BAUG"}]"#.to_vec()))));
		let ids = ["first", "second", "missing"].map(|id| GeneratedId(id.into()));
		let rows = client(rest)
			.load_multiple(&type_ref(), &GeneratedId("list".into()), &ids)
			.await
			.unwrap();
		assert_eq!(rows.len(), 2);
		assert_eq!(
			rows[0]["101"],
			ElementValue::IdTupleGeneratedElementId(IdTupleGenerated::new(
				GeneratedId("list".into()),
				ids[1].clone()
			))
		);
		assert_eq!(
			rows[1]["101"],
			ElementValue::IdTupleGeneratedElementId(IdTupleGenerated::new(
				GeneratedId("list".into()),
				ids[0].clone()
			))
		);
	}
	#[tokio::test]
	async fn chunks_at_100_and_encodes_query() {
		let sizes = Arc::new(Mutex::new(Vec::new()));
		let observed = sizes.clone();
		let mut rest = MockRestClient::new();
		rest.expect_request_binary()
			.times(2)
			.returning(move |url, _, _| {
				let query = url.split("?ids=").nth(1).unwrap();
				observed.lock().unwrap().push(query.split("%2C").count());
				assert!(!query.contains('&'));
				Ok(response(200, Some(b"[]".to_vec())))
			});
		let ids = (0..101)
			.map(|i| GeneratedId(format!("id-{i}")))
			.collect::<Vec<_>>();
		assert!(client(rest)
			.load_multiple(&type_ref(), &GeneratedId("list".into()), &ids)
			.await
			.unwrap()
			.is_empty());
		assert_eq!(*sizes.lock().unwrap(), vec![100, 1]);
	}
	#[tokio::test]
	async fn malformed_or_missing_body_is_an_error() {
		for body in [
			None,
			Some(b"{".to_vec()),
			Some(b"{}".to_vec()),
			Some(b"[{}]".to_vec()),
		] {
			let mut rest = MockRestClient::new();
			rest.expect_request_binary()
				.times(1)
				.return_once(move |_, _, _| Ok(response(200, body)));
			assert!(client(rest)
				.load_multiple(
					&type_ref(),
					&GeneratedId("list".into()),
					&[GeneratedId("id".into())]
				)
				.await
				.is_err());
		}
	}
	#[tokio::test]
	async fn failed_batch_stops_the_operation() {
		let mut rest = MockRestClient::new();
		rest.expect_request_binary()
			.times(1)
			.returning(|_, _, _| Ok(response(500, None)));
		assert!(client(rest)
			.load_multiple(
				&type_ref(),
				&GeneratedId("list".into()),
				&vec![GeneratedId("id".into()); 101]
			)
			.await
			.is_err());
	}
}
