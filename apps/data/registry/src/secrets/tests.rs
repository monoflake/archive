use super::fake::{self, Asked};
use super::*;

const VALUE: &str = "me@example.com";

#[test]
fn a_value_is_named_for_its_account_and_kind() {
	assert_eq!(name_of("a3f8c91d7e42b650", "email"), "ACCOUNT_A3F8C91D7E42B650_EMAIL");
	assert_eq!(name_of("a3f8c91d7e42b650", "username"), "ACCOUNT_A3F8C91D7E42B650_USERNAME");
	assert_eq!(
		kept_as("a3f8c91d7e42b650", "account_id"),
		"accounts/registry/ACCOUNT_A3F8C91D7E42B650_ACCOUNT_ID"
	);
}

#[tokio::test]
async fn a_value_is_written_at_the_revision_read_with_the_scope_token() {
	let (base, fake) = fake::serve().await;
	let secrets = Secrets::new(vec![base], "scoped".into());
	secrets.put("ACCOUNT_00000000000000A1_EMAIL", VALUE, "alice").await.unwrap();
	fake.lock().unwrap().revisions.insert("ACCOUNT_00000000000000A1_EMAIL".into(), 7);
	secrets.put("ACCOUNT_00000000000000A1_EMAIL", VALUE, "alice").await.unwrap();

	let asked = fake.lock().unwrap().asked.clone();
	let path = "/v1/secrets/registry/ACCOUNT_00000000000000A1_EMAIL".to_owned();
	let read = Asked {
		method: "GET",
		path: path.clone(),
		scope: Some("accounts".into()),
		bearer: "Bearer scoped".into(),
		body: Value::Null,
	};
	let write = |revision: i64| Asked {
		method: "PUT",
		body: json!({ "by": "alice", "revision": revision, "value": VALUE, "class": "secret" }),
		..read.clone()
	};
	let expected = [read.clone(), write(0), read.clone(), write(7)];
	assert_eq!(asked, expected);
}

#[tokio::test]
async fn a_refusal_is_its_code_alone_and_no_core_answering_is_unanswered() {
	let (base, fake) = fake::serve().await;
	fake.lock().unwrap().refusing = Some((401, "invalid_token"));
	let secrets = Secrets::new(vec![base.clone()], "wrong".into());
	assert_eq!(secrets.put("A", VALUE, "alice").await, Err(Untaken::Refused("invalid_token".into())));
	assert!(!Untaken::Refused("invalid_token".into()).to_string().contains(VALUE));

	let nobody = Secrets::new(vec![fake::unreachable().await], "scoped".into());
	assert_eq!(nobody.put("A", VALUE, "alice").await, Err(Untaken::Unanswered));
}

/// A core that does not answer is passed over for the next; one that answers is the one written to.
#[tokio::test]
async fn the_first_core_that_answers_takes_the_value() {
	let (base, fake) = fake::serve().await;
	let secrets = Secrets::new(vec![fake::unreachable().await, base], "scoped".into());
	secrets.put("ACCOUNT_00000000000000A1_EMAIL", VALUE, "alice").await.unwrap();
	assert_eq!(fake.lock().unwrap().revisions["ACCOUNT_00000000000000A1_EMAIL"], 1);
}
