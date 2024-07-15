use crate::sia::address::Address;
use crate::sia::blake2b_internal::{public_key_leaf, sigs_required_leaf, standard_unlock_hash, timelock_leaf,
                                   Accumulator};
use crate::sia::encoding::{Encodable, Encoder, PrefixedH256, PrefixedPublicKey};
use crate::sia::specifier::Specifier;
use ed25519_dalek::PublicKey;
use nom::branch::alt;
use nom::bytes::complete::{tag, take_while_m_n};
use nom::character::complete::{char, digit1, multispace0};
use nom::combinator::all_consuming;
use nom::combinator::map_res;
use nom::multi::separated_list0;
use nom::sequence::{delimited, preceded, separated_pair};
use nom::IResult;
use rpc::v1::types::H256;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as};
use std::str::FromStr;

// parse 32 bytes of hex to &str
fn parse_hex_str(input: &str) -> IResult<&str, &str> {
    all_consuming(take_while_m_n(64, 64, |c: char| c.is_digit(16)))(input)
}

// parse 32 bytes of hex to Vec<u8>
fn parse_hex(input: &str) -> IResult<&str, Vec<u8>> {
    all_consuming(map_res(take_while_m_n(64, 64, |c: char| c.is_digit(16)), hex::decode))(input)
}

fn parse_u64(input: &str) -> IResult<&str, u64> { map_res(digit1, |s: &str| s.parse::<u64>())(input) }

fn parse_above(input: &str) -> IResult<&str, SpendPolicy> {
    let parse_whitespace = delimited(multispace0, parse_u64, multispace0);
    let (input, value) = delimited(tag("above("), parse_whitespace, char(')'))(input)?;
    Ok((input, SpendPolicy::Above(value)))
}

fn parse_after(input: &str) -> IResult<&str, SpendPolicy> {
    let parse_whitespace = delimited(multispace0, parse_u64, multispace0);
    let (input, value) = delimited(tag("after("), parse_whitespace, char(')'))(input)?;
    Ok((input, SpendPolicy::After(value)))
}

fn parse_opaque(input: &str) -> IResult<&str, SpendPolicy> {
    let parse_hash = map_res(parse_hex_str, H256::from_str);
    let parse_prefix = preceded(tag("0x"), parse_hash);
    let parse_whitespace = delimited(multispace0, parse_prefix, multispace0);
    let (input, h256) = delimited(tag("opaque("), parse_whitespace, tag(")"))(input)?;
    Ok((input, SpendPolicy::Opaque(h256)))
}

fn parse_hash(input: &str) -> IResult<&str, SpendPolicy> {
    let parse_hash = map_res(parse_hex_str, H256::from_str);
    let parse_prefix = preceded(tag("0x"), parse_hash);
    let parse_whitespace = delimited(multispace0, parse_prefix, multispace0);
    let (input, h256) = delimited(tag("h("), parse_whitespace, tag(")"))(input)?;
    Ok((input, SpendPolicy::Hash(h256)))
}

fn parse_public_key(input: &str) -> IResult<&str, SpendPolicy> {
    let parse_public_key = map_res(parse_hex, |bytes: Vec<u8>| PublicKey::from_bytes(&bytes));
    let parse_prefix = preceded(tag("0x"), parse_public_key);
    let (input, public_key) = delimited(tag("pk("), parse_prefix, char(')'))(input)?;
    Ok((input, SpendPolicy::PublicKey(public_key)))
}

fn parse_threshold(input: &str) -> IResult<&str, SpendPolicy> {
    let parse_threshold = separated_pair(
        map_res(digit1, |s: &str| s.parse::<u8>()),
        char(','),
        delimited(tag("["), separated_list0(char(','), parse_spend_policy), tag("]")),
    );
    let (input, (n, of)) = delimited(tag("thresh("), parse_threshold, tag(")"))(input)?;
    Ok((input, SpendPolicy::Threshold { n, of }))
}

fn parse_spend_policy(input: &str) -> IResult<&str, SpendPolicy> {
    let parse_policy = alt((
        parse_above,
        parse_after,
        parse_public_key,
        parse_hash,
        parse_threshold,
        parse_opaque,
        // parse_unlock_condition, // TODO this may still be in flux from Sia devs
    ));
    // drop whitespace characters before and after the policy
    delimited(multispace0, parse_policy, multispace0)(input)
}

impl SpendPolicy {
    pub fn from_str(input: &str) -> Result<SpendPolicy, nom::Err<nom::error::Error<&str>>> {
        match all_consuming(parse_spend_policy)(input) {
            Ok((_, policy)) => Ok(policy),
            Err(e) => Err(e),
        }
    }
}

const POLICY_VERSION: u8 = 1u8;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub enum SpendPolicy {
    Above(u64),
    After(u64),
    PublicKey(PublicKey),
    Hash(H256),
    Threshold { n: u8, of: Vec<SpendPolicy> },
    Opaque(H256),
    UnlockConditions(UnlockCondition), // For v1 compatibility
}

// serde_with is used to serialize/deserialize SpendPolicy with prefixed PublicKey and H256
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", content = "policy", rename_all = "camelCase")]
pub enum SpendPolicyHelper {
    Above(u64),
    After(u64),
    Pk(PrefixedPublicKey),
    H(PrefixedH256),
    Thresh { n: u8, of: Vec<SpendPolicy> },
    Opaque(PrefixedH256),
    Uc(UnlockCondition), // For v1 compatibility
}

impl From<SpendPolicyHelper> for SpendPolicy {
    fn from(helper: SpendPolicyHelper) -> Self {
        match helper {
            SpendPolicyHelper::Above(height) => SpendPolicy::Above(height),
            SpendPolicyHelper::After(time) => SpendPolicy::After(time),
            SpendPolicyHelper::Pk(pk) => SpendPolicy::PublicKey(pk.0),
            SpendPolicyHelper::H(hash) => SpendPolicy::Hash(hash.0),
            SpendPolicyHelper::Thresh { n, of } => SpendPolicy::Threshold { n, of },
            SpendPolicyHelper::Opaque(hash) => SpendPolicy::Opaque(hash.0),
            SpendPolicyHelper::Uc(uc) => SpendPolicy::UnlockConditions(uc),
        }
    }
}

// Go serializes SpendPolicy with custom logic
// eg, "policy": "pk(0x8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c)"
// see `func (p SpendPolicy) String()` in policy.go
impl Encodable for SpendPolicy {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.write_u8(POLICY_VERSION);
        self.encode_wo_prefix(encoder);
    }
}

impl SpendPolicy {
    pub fn to_u8(&self) -> u8 {
        match self {
            SpendPolicy::Above(_) => 1,
            SpendPolicy::After(_) => 2,
            SpendPolicy::PublicKey(_) => 3,
            SpendPolicy::Hash(_) => 4,
            SpendPolicy::Threshold { n: _, of: _ } => 5,
            SpendPolicy::Opaque(_) => 6,
            SpendPolicy::UnlockConditions(_) => 7,
        }
    }

    pub fn encode_wo_prefix(&self, encoder: &mut Encoder) {
        let opcode = self.to_u8();
        match self {
            SpendPolicy::Above(height) => {
                encoder.write_u8(opcode);
                encoder.write_u64(*height);
            },
            SpendPolicy::After(time) => {
                encoder.write_u8(opcode);
                encoder.write_u64(*time);
            },
            SpendPolicy::PublicKey(pubkey) => {
                encoder.write_u8(opcode);
                encoder.write_slice(&pubkey.to_bytes());
            },
            SpendPolicy::Hash(hash) => {
                encoder.write_u8(opcode);
                encoder.write_slice(&hash.0);
            },
            SpendPolicy::Threshold { n, of } => {
                encoder.write_u8(opcode);
                encoder.write_u8(*n);
                encoder.write_u8(of.len() as u8);
                for policy in of {
                    policy.encode_wo_prefix(encoder);
                }
            },
            SpendPolicy::Opaque(address) => {
                encoder.write_u8(opcode);
                encoder.write_slice(&address.0);
            },
            SpendPolicy::UnlockConditions(unlock_condition) => {
                encoder.write_u8(opcode);
                encoder.write_u64(unlock_condition.timelock);
                encoder.write_u64(unlock_condition.unlock_keys.len() as u64);
                for uc in &unlock_condition.unlock_keys {
                    uc.encode(encoder);
                }
                encoder.write_u64(unlock_condition.sigs_required);
            },
        }
    }

    pub fn address(&self) -> Address {
        if let SpendPolicy::UnlockConditions(unlock_condition) = self {
            return unlock_condition.address();
        }
        let mut encoder = Encoder::default();
        encoder.write_distinguisher("address");

        // if self is a threshold policy, we need to convert all of its subpolicies to opaque
        let new_policy = match self {
            SpendPolicy::Threshold { n, of } => SpendPolicy::Threshold {
                n: *n,
                of: of.iter().map(SpendPolicy::opaque).collect(),
            },
            _ => self.clone(),
        };
        new_policy.encode(&mut encoder);

        Address(encoder.hash())
    }

    pub fn above(height: u64) -> Self { SpendPolicy::Above(height) }

    pub fn after(time: u64) -> Self { SpendPolicy::After(time) }

    pub fn public_key(pk: PublicKey) -> Self { SpendPolicy::PublicKey(pk) }

    pub fn hash(h: H256) -> Self { SpendPolicy::Hash(h) }

    pub fn threshold(n: u8, of: Vec<SpendPolicy>) -> Self { SpendPolicy::Threshold { n, of } }

    pub fn opaque(p: &SpendPolicy) -> Self { SpendPolicy::Opaque(p.address().0) }

    pub fn anyone_can_spend() -> Self { SpendPolicy::threshold(0, vec![]) }
}

pub fn opacify_policy(p: &SpendPolicy) -> SpendPolicy { SpendPolicy::Opaque(p.address().0) }

pub fn spend_policy_atomic_swap(alice: PublicKey, bob: PublicKey, lock_time: u64, hash: H256) -> SpendPolicy {
    let policy_after = SpendPolicy::After(lock_time);
    let policy_hash = SpendPolicy::Hash(hash);

    let policy_success = SpendPolicy::Threshold {
        n: 2,
        of: vec![SpendPolicy::PublicKey(alice), policy_hash],
    };

    let policy_refund = SpendPolicy::Threshold {
        n: 2,
        of: vec![SpendPolicy::PublicKey(bob), policy_after],
    };

    SpendPolicy::Threshold {
        n: 1,
        of: vec![policy_success, policy_refund],
    }
}

pub fn spend_policy_atomic_swap_success(alice: PublicKey, bob: PublicKey, lock_time: u64, hash: H256) -> SpendPolicy {
    match spend_policy_atomic_swap(alice, bob, lock_time, hash) {
        SpendPolicy::Threshold { n, mut of } => {
            of[1] = opacify_policy(&of[1]);
            SpendPolicy::Threshold { n, of }
        },
        _ => unreachable!(),
    }
}

pub fn spend_policy_atomic_swap_refund(alice: PublicKey, bob: PublicKey, lock_time: u64, hash: H256) -> SpendPolicy {
    match spend_policy_atomic_swap(alice, bob, lock_time, hash) {
        SpendPolicy::Threshold { n, mut of } => {
            of[0] = opacify_policy(&of[0]);
            SpendPolicy::Threshold { n, of }
        },
        _ => unreachable!(),
    }
}

// Sia Go v1 technically supports arbitrary length public keys
// We only support ed25519 but must be able to deserialize others
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum UnlockKey {
    Ed25519(PublicKey),
    Unsupported { algorithm: Specifier, public_key: Vec<u8> },
}

impl Encodable for PublicKey {
    fn encode(&self, encoder: &mut Encoder) { encoder.write_slice(&self.to_bytes()); }
}

impl Encodable for UnlockKey {
    fn encode(&self, encoder: &mut Encoder) {
        match self {
            UnlockKey::Ed25519(public_key) => {
                Specifier::Ed25519.encode(encoder);
                encoder.write_u64(32); // ed25519 public key length
                public_key.encode(encoder);
            },
            UnlockKey::Unsupported { algorithm, public_key } => {
                algorithm.encode(encoder);
                encoder.write_u64(public_key.len() as u64);
                encoder.write_slice(public_key);
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UnlockCondition {
    pub unlock_keys: Vec<UnlockKey>,
    pub timelock: u64,
    pub sigs_required: u64,
}

impl Encodable for UnlockCondition {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.write_u64(self.timelock);
        encoder.write_u64(self.unlock_keys.len() as u64);
        for unlock_key in &self.unlock_keys {
            unlock_key.encode(encoder);
        }
        encoder.write_u64(self.sigs_required);
    }
}

impl UnlockCondition {
    pub fn new(pubkeys: Vec<PublicKey>, timelock: u64, sigs_required: u64) -> Self {
        // TODO check go implementation to see if there should be limitations or checks imposed here
        // eg, max number of keys, max sigs_required, etc
        let unlock_keys = pubkeys
            .into_iter()
            .map(|public_key| UnlockKey::Ed25519(public_key))
            .collect();

        UnlockCondition {
            unlock_keys,
            timelock,
            sigs_required,
        }
    }

    pub fn standard_unlock(public_key: PublicKey) -> Self {
        UnlockCondition {
            unlock_keys: vec![UnlockKey::Ed25519(public_key)],
            timelock: 0,
            sigs_required: 1,
        }
    }

    pub fn unlock_hash(&self) -> H256 {
        // almost all UnlockConditions are standard, so optimize for that case
        if let UnlockKey::Ed25519(public_key) = &self.unlock_keys[0] {
            if self.timelock == 0 && self.unlock_keys.len() == 1 && self.sigs_required == 1 {
                return standard_unlock_hash(&public_key);
            }
        }

        let mut accumulator = Accumulator::default();

        accumulator.add_leaf(timelock_leaf(self.timelock));

        for unlock_key in &self.unlock_keys {
            accumulator.add_leaf(public_key_leaf(&unlock_key));
        }

        accumulator.add_leaf(sigs_required_leaf(self.sigs_required));
        accumulator.root()
    }

    pub fn address(&self) -> Address { Address(self.unlock_hash()) }
}

#[test]
fn test_unlock_condition_unlock_hash_standard() {
    let pubkey = PublicKey::from_bytes(
        &hex::decode("0102030000000000000000000000000000000000000000000000000000000000").unwrap(),
    )
    .unwrap();
    let unlock_condition = UnlockCondition::new(vec![pubkey], 0, 1);

    let hash = unlock_condition.unlock_hash();
    let expected = H256::from("72b0762b382d4c251af5ae25b6777d908726d75962e5224f98d7f619bb39515d");
    assert_eq!(hash, expected);

    let hash = standard_unlock_hash(&pubkey);
    assert_eq!(hash, expected);
}

#[test]
fn test_unlock_condition_unlock_hash_2of2_multisig() {
    let pubkey = PublicKey::from_bytes(
        &hex::decode("0102030000000000000000000000000000000000000000000000000000000000").unwrap(),
    )
    .unwrap();
    let pubkey2 = PublicKey::from_bytes(
        &hex::decode("0101010000000000000000000000000000000000000000000000000000000000").unwrap(),
    )
    .unwrap();
    let unlock_condition = UnlockCondition::new(vec![pubkey, pubkey2], 0, 2);

    let hash = unlock_condition.unlock_hash();
    let expected = H256::from("1e94357817d236167e54970a8c08bbd41b37bfceeeb52f6c1ce6dd01d50ea1e7");
    assert_eq!(hash, expected);
}

#[test]
fn test_unlock_condition_unlock_hash_1of2_multisig() {
    let pubkey = PublicKey::from_bytes(
        &hex::decode("0102030000000000000000000000000000000000000000000000000000000000").unwrap(),
    )
    .unwrap();
    let pubkey2 = PublicKey::from_bytes(
        &hex::decode("0101010000000000000000000000000000000000000000000000000000000000").unwrap(),
    )
    .unwrap();
    let unlock_condition = UnlockCondition::new(vec![pubkey, pubkey2], 0, 1);

    let hash = unlock_condition.unlock_hash();
    let expected = H256::from("d7f84e3423da09d111a17f64290c8d05e1cbe4cab2b6bed49e3a4d2f659f0585");
    assert_eq!(hash, expected);
}

#[test]
fn test_spend_policy_encode_above() {
    let policy = SpendPolicy::above(1);

    let hash = Encoder::encode_and_hash(&policy);
    let expected = H256::from("bebf6cbdfb440a92e3e5d832ac30fe5d226ff6b352ed3a9398b7d35f086a8ab6");
    assert_eq!(hash, expected);

    let address = policy.address();
    let expected =
        Address::from_str("addr:188b997bb99dee13e95f92c3ea150bd76b3ec72e5ba57b0d57439a1a6e2865e9b25ea5d1825e").unwrap();
    assert_eq!(address, expected);
}

#[test]
fn test_spend_policy_encode_after() {
    let policy = SpendPolicy::after(1);
    let hash = Encoder::encode_and_hash(&policy);
    let expected = H256::from("07b0f28eafd87a082ad11dc4724e1c491821260821a30bec68254444f97d9311");
    assert_eq!(hash, expected);

    let address = policy.address();
    let expected =
        Address::from_str("addr:60c74e0ce5cede0f13f83b0132cb195c995bc7688c9fac34bbf2b14e14394b8bbe2991bc017f").unwrap();
    assert_eq!(address, expected);
}

#[test]
fn test_spend_policy_encode_pubkey() {
    let pubkey = PublicKey::from_bytes(
        &hex::decode("0102030000000000000000000000000000000000000000000000000000000000").unwrap(),
    )
    .unwrap();
    let policy = SpendPolicy::PublicKey(pubkey);

    let hash = Encoder::encode_and_hash(&policy);
    let expected = H256::from("4355c8f80f6e5a98b70c9c2f9a22f17747989b4744783c90439b2b034f698bfe");
    assert_eq!(hash, expected);

    let address = policy.address();
    let expected =
        Address::from_str("addr:55a7793237722c6df8222fd512063cb74228085ef1805c5184713648c159b919ac792fbad0e1").unwrap();
    assert_eq!(address, expected);
}

#[test]
fn test_spend_policy_encode_hash() {
    let hash = H256::from("0102030000000000000000000000000000000000000000000000000000000000");
    let policy = SpendPolicy::Hash(hash);

    let hash = Encoder::encode_and_hash(&policy);
    let expected = H256::from("9938967aefa6cbecc1f1620d2df5170d6811d4b2f47a879b621c1099a3b0628a");
    assert_eq!(hash, expected);

    let address = policy.address();
    let expected =
        Address::from_str("addr:a4d5a06d8d3c2e45aa26627858ce8e881505ae3c9d122a1d282c7824163751936cffb347e435").unwrap();
    assert_eq!(address, expected);
}

#[test]
fn test_spend_policy_encode_threshold() {
    let policy = SpendPolicy::Threshold {
        n: 1,
        of: vec![SpendPolicy::above(1), SpendPolicy::after(1)],
    };

    let hash = Encoder::encode_and_hash(&policy);
    let expected = H256::from("7d792df6cd0b5e0f795287b3bf4087bbcc4c1bd0c52880a552cdda3e5e33d802");
    assert_eq!(hash, expected);

    let address = policy.address();
    let expected =
        Address::from_str("addr:4179b53aba165e46e4c85b3c8766bb758fb6f0bfa5721550b81981a3ec38efc460557dc1ded4").unwrap();
    assert_eq!(address, expected);
}

#[test]
fn test_spend_policy_encode_unlock_condition() {
    let pubkey = PublicKey::from_bytes(
        &hex::decode("0102030000000000000000000000000000000000000000000000000000000000").unwrap(),
    )
    .unwrap();
    let unlock_condition = UnlockCondition::new(vec![pubkey], 0, 1);

    let sub_policy = SpendPolicy::UnlockConditions(unlock_condition);
    let base_address = sub_policy.address();
    let expected =
        Address::from_str("addr:72b0762b382d4c251af5ae25b6777d908726d75962e5224f98d7f619bb39515dd64b9a56043a").unwrap();
    assert_eq!(base_address, expected);

    let policy = SpendPolicy::Threshold {
        n: 1,
        of: vec![sub_policy],
    };
    let address = policy.address();
    let expected =
        Address::from_str("addr:1498a58c843ce66740e52421632d67a0f6991ea96db1fc97c29e46f89ae56e3534078876331d").unwrap();
    assert_eq!(address, expected);
}

#[test]
fn test_unlock_condition_encode() {
    let pubkey = PublicKey::from_bytes(
        &hex::decode("0102030000000000000000000000000000000000000000000000000000000000").unwrap(),
    )
    .unwrap();
    let unlock_condition = UnlockCondition::new(vec![pubkey], 0, 1);

    let hash = Encoder::encode_and_hash(&unlock_condition);
    let expected = H256::from("5d49bae37b97c86573a1525246270c180464acf33d63cc2ac0269ef9a8cb9d98");
    assert_eq!(hash, expected);
}

#[test]
fn test_public_key_encode() {
    let public_key = PublicKey::from_bytes(
        &hex::decode("0102030000000000000000000000000000000000000000000000000000000000").unwrap(),
    )
    .unwrap();

    let hash = Encoder::encode_and_hash(&public_key);
    let expected = H256::from("d487326614f066416308bf6aa4e5041d1949928e4b26ede98e3cebb36a3b1726");
    assert_eq!(hash, expected);
}

