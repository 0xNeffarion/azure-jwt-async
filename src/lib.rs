//! # A library that authenticates Azure JWT tokens.
//!
//! This library will fetch public keys from Microsoft and validate the authenticity of the Tokens 
//! and verify that they are issued by Azure and are not tampered with.
//!
//! It will also check that this token is issued to the right audience matching the `aud` property 
//! of the token with the client_id you got when you registered your app in Azure. If either of 
//! these fail, the token is invalid.
//! 
//! # Dafault validation
//! 
//! **There are mainly five conditions a well formed token will need to meet to be validated:**
//! 1. That the token is issued by Azure and is not tampered with
//! 2. That this token is issued for use in your application
//! 3. That the token is not expired
//! 4. That the token is not used before it's valid
//! 5. That the token is not issued in the future
//! 6. That the algorithm the token tells us to use is the same as we use*
//! 
//! * Note that we do NOT use the token header to set the algorithm for us, look [at this article 
//! for more information on why that would be bad](https://auth0.com/blog/critical-vulnerabilities-in-json-web-token-libraries/)
//! 
//! The validation will `Error` on a failed validation providing more granularity for library users 
//! to find out why the token was rejected.
//!
//! If the token is invalid it will return an Error instead of a boolean. The main reason for this 
//! is easier logging of what type of test it failed.
//!
//! # Security
//! You will need a private app_id created by Azure for your application to be able to veriify that
//! the token is created for your application (and not anyone with a valid Azure token can log in)
//! and you will need to authenticate that the user has the right access to your system.
//!
//! For more information, see this artice: https://docs.microsoft.com/en-us/azure/active-directory/develop/id-tokens
use base64;
use chrono::{Duration, Local, NaiveDateTime};
use jsonwebtoken as jwt;
use reqwest::{self, Response};
use serde::{Deserialize, Serialize};

mod error;
pub use error::AuthErr;

const AZ_OPENID_URL: &str =
    "https://login.microsoftonline.com/common/.well-known/openid-configuration";

/// AzureAuth is the what you'll use to validate your token. I'll briefly explain here what 
/// defaults are set and which you can change:
///
/// # Defaults
/// 
/// - Public key expiration: dafault set to 24h, use `set_expiration` to set a different expiration 
///   in hours.
/// - Hashing algorithm: Sha256, you can't change this setting. Submit an issue in the github repo 
///   if this is important to you
/// - Retry on no match. If no matching key is found and our keys are older than an hour, we 
///   refresh the keys and try once more. Limited to once in an hour. You can disable this by 
///   calling `set_no_retry()`.
/// - The timestamps are given a 60s "leeway" to account for time skew between servers
///
/// # Errors:
/// - If one of Microsofts enpoints for public keys are down
/// - If the token can't be parsed as a valid Azure token
/// - If the tokens fails it's authenticity test
/// - If the token is invalid
#[derive(Debug, Clone)]
pub struct AzureAuth {
    aud_to_val: String,
    jwks_uri: String,
    public_keys: Option<Vec<KeyPairs>>,
    last_refresh: Option<NaiveDateTime>,
    exp_hours: i64,
    retry_counter: u32,
    retry_option: bool,
    is_offline: bool,
}

impl AzureAuth {
    /// One thing to note that this method will call the Microsoft apis to fetch the current keys 
    /// an this can fail. The public keys are fetched since we will not be able to perform any 
    /// verification without them. Please note that this method is quite expensive to do. Try 
    /// keeping the object alive instead of creating new objects. If you need to pass around an 
    /// instance of the object, then cloning it will be cheaper than creating a new one.
    ///
    /// # Errors
    /// If there is a connection issue to the Microsoft public key apis.
    pub fn new(aud: impl Into<String>) -> Result<Self, AuthErr> {
        Ok(AzureAuth {
            aud_to_val: aud.into(),
            jwks_uri: AzureAuth::get_jwks_uri()?,
            public_keys: None,
            last_refresh: None,
            exp_hours: 24,
            retry_counter: 0,
            retry_option: true,
            is_offline: false,
        })
    }

    /// If you want to handle updating the public keys yourself
    fn new_offline(
        aud: impl Into<String>,
        public_keys: Vec<KeyPairs>,
    ) -> Result<Self, AuthErr> {
        Ok(AzureAuth {
            aud_to_val: aud.into(),
            jwks_uri: AzureAuth::get_jwks_uri()?,
            public_keys: Some(public_keys),
            last_refresh: Some(Local::now().naive_local()),
            exp_hours: 24,
            retry_counter: 0,
            retry_option: true,
            is_offline: true,
        })
    }

    /// Dafault validation, see struct documentation for the defaults.
    pub fn validate_token(&mut self, token: &str) -> Result<Token<AzureJwtClaims>, AuthErr> {
        let mut validator = jwt::Validation::new(jwt::Algorithm::RS256);

        // exp, nbf, iat is set to validate as default
        validator.leeway = 60;
        validator.set_audience(&self.aud_to_val);
        let decoded: Token<AzureJwtClaims> = self.validate_token_authenticity(token, &validator)?;

        Ok(decoded)
    }

    /// Allows for a custom validator and mapping the token to your own type.
    /// Useful in situations where you get fields you that are not covered by 
    /// the default mapping or want to change the validaion requirements (i.e 
    /// if you want the leeway set to two minutes instead of one).
    /// 
    /// # Example
    /// 
    /// ```rust,ignore
    /// use azure_oauth_r1s::*;
    /// use jsonwebtoken::{Validation, Token};
    /// use serde::{Seralize, Deserialize};
    /// 
    /// let mut validator = Validation::new();
    /// validator.leeway = 120;
    /// 
    /// #[derive(Serialize, Deserialize)]
    /// struct MyClaims {
    ///     group: String,
    ///     roles: Vec<String>,
    /// }
    /// 
    /// let auth = AzureAuth::new(my_client_id_from_azure).unwrap();
    /// 
    /// let valid_token: Token<MyClaims>  = auth.validate_custom(some_token, &validator).unwrap();
    /// ```
    /// 
    /// You'll need to pull in `jsonwebtoken` crate to 
    pub fn validate_custom<T>(
        &mut self,
        token: &str,
        validator: &jwt::Validation,
    ) -> Result<Token<T>, AuthErr>
    where
        for<'de> T: Serialize + Deserialize<'de>,
    {
        let decoded: Token<T> = self.validate_token_authenticity(token, &validator)?;
        Ok(decoded)
    }

    fn validate_token_authenticity<T>(
        &mut self,
        token: &str,
        validator: &jwt::Validation,
    ) -> Result<Token<T>, AuthErr>
    where
        for<'de> T: Serialize + Deserialize<'de>,
    {
        // if we´re in offline, we never refresh the keys. It's up to the user to do that.
        if !self.is_keys_valid() && !self.is_offline {
            self.refresh_pub_keys()?;
        }
        // does not validate the token!
        let decoded = jwt::decode_header(token)?;

        let key = match &self.public_keys {
            None => {
                return Err(
                    AuthErr::Other("Internal err. No public keys found.".into(),
                ))
            }
            Some(keys) => match &decoded.kid {
                None => return Err(AuthErr::Other("No `kid` in token.".into())),
                Some(kid) => keys.iter().find(|k| k.x5t == *kid),
            },
        };

        // The token should pr specification use RS256, if it's not it has been
        // tampered with or the header is wrong. In that case we invalidate the
        // token.
        // NOTE: needs to be updated if Microsoft changes their spec
        if decoded.alg != jwt::Algorithm::RS256 {
            return Err(
                AuthErr::Other("Invalid token. Invalid algorithm in header.".into(),
                    ));
        }

        let auth_key = match key {
            None => {
                // the first time this happens let's go and refresh the keys and try once more.
                // It could be that our keys are out of date. Limit to once in an hour.
                if self.should_retry() {
                    self.refresh_pub_keys()?;
                    self.retry_counter += 1;
                    self.validate_token(token)?;
                    unreachable!()
                } else {
                    self.retry_counter = 0;
                    return Err(
                        AuthErr::Other("Invalid token. Could not verify authenticity.".into(),
                    ));
                }
            }
            Some(key) => {
                self.retry_counter = 0;
                key
            }
        };

        // the jwt library expects a byte input so we need to decode the
        // base64 data to an bytearray
        let key_as_bytes = from_base64_to_bytearray(&auth_key.x5c[0])?;

        let valid: Token<T> = jwt::decode(token, &key_as_bytes, &validator)?;

        Ok(valid)
    }

    fn should_retry(&mut self) -> bool {
        if self.is_offline {
            return false;
        }

        match &self.last_refresh {
            Some(lr) => {
                self.retry_counter == 0 && Local::now().naive_local() - *lr > Duration::hours(1)
            }
            None => false,
        }
    }

    /// Sets the expiration of the cached public keys in hours. Pr. 04.2019 Microsoft rotates these 
    /// every 24h.
    pub fn set_expiration(&mut self, hours: i64) {
        self.exp_hours = hours;
    }

    pub fn set_no_retry(&mut self) {
        self.retry_option = false;
    }

    fn is_keys_valid(&self) -> bool {
        match self.last_refresh {
            None => false,
            Some(dt) => Local::now().naive_local() - dt <= Duration::hours(self.exp_hours),
        }
    }

    fn refresh_pub_keys(&mut self) -> Result<(), AuthErr> {
        let mut resp: Response =
            reqwest::get(&self.jwks_uri)?;
        let resp: Keys = resp.json()?;
        self.last_refresh = Some(Local::now().naive_local());
        self.public_keys = Some(resp.keys);
        Ok(())
    }

    fn refresh_rwks_uri(&mut self) -> Result<(), AuthErr> {
        self.jwks_uri = AzureAuth::get_jwks_uri()?;
        Ok(())
    }

    fn get_jwks_uri() -> Result<String, AuthErr> {
        let mut resp: Response =
            reqwest::get(AZ_OPENID_URL)?;
        let resp: OpenIdResponse = resp.json()?;

        Ok(resp.jwks_uri)
    }

    /// If you use the "offline" variant you'll need this to update the public keys, if you don't
    /// use the offline version you probably don't want to change these unless you're testing.
    pub fn set_public_keys(&mut self, pub_keys: Vec<KeyPairs>) {
        self.last_refresh = Some(Local::now().naive_local());
        self.public_keys = Some(pub_keys);
    }
}

pub struct AzureJwtHeader {
    /// Indicates that the token is a JWT.
    pub typ: String,
    /// Indicates the algorithm that was used to sign the token. Example: "RS256"
    pub alg: String,
    /// Thumbprint for the public key used to sign this token. Emitted in both
    /// v1.0 and v2.0 id_tokens
    pub kid: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AzureJwtClaims {
    /// dentifies the intended recipient of the token. In id_tokens, the audience
    /// is your app's Application ID, assigned to your app in the Azure portal.
    /// Your app should validate this value, and reject the token if the value
    /// does not match.
    pub aud: String,

    /// The application ID of the client using the token. The application can
    /// act as itself or on behalf of a user. The application ID typically
    /// represents an application object, but it can also represent a service
    /// principal object in Azure AD.
    pub azp: Option<String>,

    /// Indicates how the client was authenticated. For a public client, the
    /// value is "0". If client ID and client secret are used, the value is "1".
    /// If a client certificate was used for authentication, the value is "2".
    pub azpacr: Option<String>,

    /// Identifies the security token service (STS) that constructs and returns
    /// the token, and the Azure AD tenant in which the user was authenticated.
    /// If the token was issued by the v2.0 endpoint, the URI will end in /v2.0.
    /// The GUID that indicates that the user is a consumer user from a Microsoft
    /// account is 9188040d-6c67-4c5b-b112-36a304b66dad.
    ///
    /// Your app should use the GUID portion of the claim to restrict the set of
    /// tenants that can sign in to the app, if applicable.
    pub iss: String,

    /// Unix timestamp. "Issued At" indicates when the authentication for this
    /// token occurred.
    pub iat: u64,

    /// Records the identity provider that authenticated the subject of the token.
    /// This value is identical to the value of the Issuer claim unless the user
    /// account not in the same tenant as the issuer - guests, for instance. If
    /// the claim isn't present, it means that the value of iss can be used
    /// instead. For personal accounts being used in an organizational context
    /// (for instance, a personal account invited to an Azure AD tenant), the idp
    /// claim may be 'live.com' or an STS URI containing the Microsoft account
    /// tenant 9188040d-6c67-4c5b-b112-36a304b66dad
    pub idp: Option<String>,

    /// Unix timestamp. The "nbf" (not before) claim identifies the time before
    /// which the JWT MUST NOT be accepted for processing.
    pub nbf: u64,

    /// Unix timestamp. he "exp" (expiration time) claim identifies the
    /// expiration time on or after which the JWT MUST NOT be accepted for
    /// processing. It's important to note that a resource may reject the token
    /// before this time as well - if, for example, a change in authentication
    /// is required or a token revocation has been detected.
    pub exp: u64,

    /// The code hash is included in ID tokens only when the ID token is issued
    /// with an OAuth 2.0 authorization code. It can be used to validate the
    /// authenticity of an authorization code. For details about performing this
    /// validation, see the OpenID Connect specification.
    pub c_hash: Option<String>,

    /// The access token hash is included in ID tokens only when the ID token is
    /// issued with an OAuth 2.0 access token. It can be used to validate the
    /// authenticity of an access token. For details about performing this
    /// validation, see the OpenID Connect specification.
    pub at_hash: Option<String>,

    /// The email claim is present by default for guest accounts that have an
    /// email address. Your app can request the email claim for managed users
    /// (those from the same tenant as the resource) using the email optional
    /// claim. On the v2.0 endpoint, your app can also request the email OpenID
    /// Connect scope - you don't need to request both the optional claim and
    /// the scope to get the claim. The email claim only supports addressable
    /// mail from the user's profile information.
    pub preferred_username: String,

    /// The name claim provides a human-readable value that identifies the
    /// subject of the token. The value isn't guaranteed to be unique, it is
    /// mutable, and it's designed to be used only for display purposes. The
    /// profile scope is required to receive this claim.
    pub name: Option<String>,

    /// The nonce matches the parameter included in the original /authorize
    /// request to the IDP. If it does not match, your application should reject
    /// the token.
    pub nonce: Option<String>,

    /// Guid. The immutable identifier for an object in the Microsoft identity system,
    /// in this case, a user account. This ID uniquely identifies the user
    /// across applications - two different applications signing in the same
    /// user will receive the same value in the oid claim. The Microsoft Graph
    /// will return this ID as the id property for a given user account. Because
    /// the oid allows multiple apps to correlate users, the profile scope is
    /// required to receive this claim. Note that if a single user exists in
    /// multiple tenants, the user will contain a different object ID in each
    /// tenant - they're considered different accounts, even though the user
    /// logs into each account with the same credentials.
    pub oid: String,

    /// The set of roles that were assigned to the user who is logging in.
    pub roles: Option<Vec<String>>,

    /// The set of scopes exposed by your application for which the client
    /// application has requested (and received) consent. Your app should verify
    /// that these scopes are valid ones exposed by your app, and make authorization
    /// decisions based on the value of these scopes. Only included for user tokens.
    pub scp: Option<String>,

    /// The principal about which the token asserts information, such as the
    /// user of an app. This value is immutable and cannot be reassigned or
    /// reused. The subject is a pairwise identifier - it is unique to a
    /// particular application ID. If a single user signs into two different
    /// apps using two different client IDs, those apps will receive two
    /// different values for the subject claim. This may or may not be wanted
    /// depending on your architecture and privacy requirements.
    pub sub: String,

    /// A GUID that represents the Azure AD tenant that the user is from.
    /// For work and school accounts, the GUID is the immutable tenant ID of
    /// the organization that the user belongs to. For personal accounts,
    /// the value is 9188040d-6c67-4c5b-b112-36a304b66dad. The profile scope is
    /// required to receive this claim.
    pub tid: String,

    /// Provides a human readable value that identifies the subject of the
    /// token. This value isn't guaranteed to be unique within a tenant and
    /// should be used only for display purposes. Only issued in v1.0 id_tokens.
    pub unique_name: Option<String>,

    /// Indicates the version of the id_token. Either 1.0 or 2.0.
    pub ver: String,
}

fn from_base64_to_bytearray(b64_str: &str) -> Result<Vec<u8>, AuthErr> {
    let decoded = base64::decode_config(b64_str, base64::STANDARD)
        .map_err(|e| AuthErr::ParseError(e.to_string()))?;
    Ok(decoded)
}

#[derive(Debug, Deserialize)]
struct Keys {
    keys: Vec<KeyPairs>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KeyPairs {
    pub x5t: String,
    pub x5c: Vec<String>,
}

#[derive(Deserialize)]
struct OpenIdResponse {
    jwks_uri: String,
}

type Token<T> = jwt::TokenData<T>;

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC_KEY_TEST: &str = 
    "MIIBCgKCAQEAyRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4\
    l4sggh5/CYYi/cvI+SXVT9kPWSKXxJXBXd/4LkvcPuUakBoAkfh+eiFVMh2VrUyW\
    yj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG\
    /AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4l\
    QzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi+yUod+j8MtvIj812dkS4QMiRVN/by2h\
    3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQIDAQAB";

    const PRIVATE_KEY_TEST: &str =
    "MIIEpAIBAAKCAQEAyRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTL\
    UTv4l4sggh5/CYYi/cvI+SXVT9kPWSKXxJXBXd/4LkvcPuUakBoAkfh+eiFVMh2V\
    rUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8H\
    oGfG/AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBI\
    Mc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi+yUod+j8MtvIj812dkS4QMiRVN/\
    by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQIDAQABAoIBAHREk0I0O9DvECKd\
    WUpAmF3mY7oY9PNQiu44Yaf+AoSuyRpRUGTMIgc3u3eivOE8ALX0BmYUO5JtuRNZ\
    Dpvt4SAwqCnVUinIf6C+eH/wSurCpapSM0BAHp4aOA7igptyOMgMPYBHNA1e9A7j\
    E0dCxKWMl3DSWNyjQTk4zeRGEAEfbNjHrq6YCtjHSZSLmWiG80hnfnYos9hOr5Jn\
    LnyS7ZmFE/5P3XVrxLc/tQ5zum0R4cbrgzHiQP5RgfxGJaEi7XcgherCCOgurJSS\
    bYH29Gz8u5fFbS+Yg8s+OiCss3cs1rSgJ9/eHZuzGEdUZVARH6hVMjSuwvqVTFaE\
    8AgtleECgYEA+uLMn4kNqHlJS2A5uAnCkj90ZxEtNm3E8hAxUrhssktY5XSOAPBl\
    xyf5RuRGIImGtUVIr4HuJSa5TX48n3Vdt9MYCprO/iYl6moNRSPt5qowIIOJmIjY\
    2mqPDfDt/zw+fcDD3lmCJrFlzcnh0uea1CohxEbQnL3cypeLt+WbU6kCgYEAzSp1\
    9m1ajieFkqgoB0YTpt/OroDx38vvI5unInJlEeOjQ+oIAQdN2wpxBvTrRorMU6P0\
    7mFUbt1j+Co6CbNiw+X8HcCaqYLR5clbJOOWNR36PuzOpQLkfK8woupBxzW9B8gZ\
    mY8rB1mbJ+/WTPrEJy6YGmIEBkWylQ2VpW8O4O0CgYEApdbvvfFBlwD9YxbrcGz7\
    MeNCFbMz+MucqQntIKoKJ91ImPxvtc0y6e/Rhnv0oyNlaUOwJVu0yNgNG117w0g4\
    t/+Q38mvVC5xV7/cn7x9UMFk6MkqVir3dYGEqIl/OP1grY2Tq9HtB5iyG9L8NIam\
    QOLMyUqqMUILxdthHyFmiGkCgYEAn9+PjpjGMPHxL0gj8Q8VbzsFtou6b1deIRRA\
    2CHmSltltR1gYVTMwXxQeUhPMmgkMqUXzs4/WijgpthY44hK1TaZEKIuoxrS70nJ\
    4WQLf5a9k1065fDsFZD6yGjdGxvwEmlGMZgTwqV7t1I4X0Ilqhav5hcs5apYL7gn\
    PYPeRz0CgYALHCj/Ji8XSsDoF/MhVhnGdIs2P99NNdmo3R2Pv0CuZbDKMU559LJH\
    UvrKS8WkuWRDuKrz1W/EQKApFjDGpdqToZqriUFQzwy7mR3ayIiogzNtHcvbDHx8\
    oFnGY0OFksX/ye0/XGpy2SFxYRwGU98HPYeBvAQQrVjdkzfy7BmXQQ==";

    fn test_token_header() -> String {
        format!(
            r#"{{
                "typ": "JWT",
                "alg": "RS256",
                "kid": "i6lGk3FZzxRcUb2C3nEQ7syHJlY"
            }}"#
        )
    }

    fn test_token_claims() -> String {
        format!(
            r#"{{
                "aud": "6e74172b-be56-4843-9ff4-e66a39bb12e3",
                "iss": "https://login.microsoftonline.com/72f988bf-86f1-41af-91ab-2d7cd011db47/v2.0",
                "iat": {},
                "nbf": {},
                "exp": {},
                "aio": "AXQAi/8IAAAAtAaZLo3ChMif6KOnttRB7eBq4/DccQzjcJGxPYy/C3jDaNGxXd6wNIIVGRghNRnwJ1lOcAnNZcjvkoyrFxCttv33140RioOFJ4bCCGVuoCag1uOTT22222gHwLPYQ/uf79QX+0KIijdrmp69RctzmQ==",
                "azp": "6e74172b-be56-4843-9ff4-e66a39bb12e3",
                "name": "Abe Lincoln",
                "azpacr": "0",
                "oid": "690222be-ff1a-4d56-abd1-7e4f7d38e474",
                "preferred_username": "abeli@microsoft.com",
                "rh": "I",
                "scp": "access_as_user",
                "sub": "HKZpfaHyWadeOouYlitjrI-KffTm222X5rrV3xDqfKQ",
                "tid": "72f988bf-86f1-41af-91ab-2d7cd011db47",
                "uti": "fqiBqXLPj0eQa82S-IYFAA",
                "ver": "2.0"
            }}"#, 
        chrono::Utc::now().timestamp() - 1000,
        chrono::Utc::now().timestamp() - 2000,
        chrono::Utc::now().timestamp() + 1000)
    }

    // We create a test token from parts here. We use the v2 token used as example
    // in https://docs.microsoft.com/en-us/azure/active-directory/develop/id-tokens
    fn generate_test_token() -> String {
        // jwt library expects a `*.der` key wich is a byte encoded file so
        // we need to convert the key from base64 to their byte value to use them.
        let private_key = from_base64_to_bytearray(PRIVATE_KEY_TEST).expect("priv_key");

        // we need to construct the calims in a function since we need to set
        // the expiration relative to current time
        let test_token_playload = test_token_claims();
        let test_token_header = test_token_header();

        // we base64 (url-safe-base64) the header and claims and arrange
        // as a jwt payload -> header_as_base64.claims_as_base64
        let test_token = [
            base64::encode_config(&test_token_header, base64::URL_SAFE),
            base64::encode_config(&test_token_playload, base64::URL_SAFE),
        ]
        .join(".");

        // we create the signature using our private key
        let signature = jwt::sign(&test_token, &private_key, jwt::Algorithm::RS256).unwrap();

        let public_key = from_base64_to_bytearray(PUBLIC_KEY_TEST).expect("publ_key");

        // we construct a complete token which looks like: header.claims.signature
        let complete_token = format!("{}.{}", test_token, signature);

        // we verify the signature here as well to catch errors in our testing
        // code early
        let verified = jwt::verify(&signature, &test_token, &public_key, jwt::Algorithm::RS256)
            .expect("verified");
        assert!(verified);

        complete_token
    }

    #[test]
    fn decode_token() {
        let token = generate_test_token();

        // we need to construct our own key object that matches on `kid` field
        // just as it should if we used the fetched keys from microsofts servers
        // since our validation methods converts the base64 data to bytes for us
        // we don't need to worry about that here.
        let key = KeyPairs {
            x5t: "i6lGk3FZzxRcUb2C3nEQ7syHJlY".to_string(),
            x5c: vec![PUBLIC_KEY_TEST.to_string()],
        };

        let mut az_auth =
            AzureAuth::new_offline("6e74172b-be56-4843-9ff4-e66a39bb12e3", vec![key]).unwrap();

        az_auth.validate_token(&token).unwrap();
    }

    // #[test]
    // TODO: Refactor to make testing easier.
    fn decode_token_retry() {
        let token = generate_test_token();
        let key = KeyPairs {
            x5t: "Xey1".to_string(),
            x5c: vec!["azure_auth_test".to_string()],
        };

        let mut az_auth = AzureAuth::new("6e74172b-be56-4843-9ff4-e66a39bb12e3").unwrap();
        az_auth.public_keys = Some(vec![key]);
        az_auth.last_refresh = Some(Local::now().naive_local() - Duration::hours(2));
        az_auth.validate_token(&token).unwrap();
    }

    #[test]
    fn refresh_rwks_uri() {
        let _az_auth = AzureAuth::new("app_secret").unwrap();
    }

    #[test]
    fn azure_ad_get_public_keys() {
        let mut az_auth = AzureAuth::new("app_secret").unwrap();
        az_auth.refresh_pub_keys().unwrap();
    }

    #[test]
    fn is_not_valid_more_than_24h() {
        let mut az_auth = AzureAuth::new("app_secret").unwrap();
        az_auth.last_refresh = Some(Local::now().naive_local() - Duration::hours(25));

        assert!(!az_auth.is_keys_valid());
    }

}
