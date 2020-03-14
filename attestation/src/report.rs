// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

// Insert std prelude in the top for the sgx feature
#[cfg(feature = "mesalock_sgx")]
use std::prelude::v1::*;

use crate::AttestationError;
use crate::EndorsedAttestationReport;
use anyhow::{anyhow, bail, ensure};
use anyhow::{Error, Result};
use chrono::DateTime;
use rustls;
use serde_json;
use serde_json::Value;
use std::convert::TryFrom;
use std::time::*;

#[cfg(feature = "mesalock_sgx")]
use std::untrusted::time::SystemTimeEx;

use uuid::Uuid;

type SignatureAlgorithms = &'static [&'static webpki::SignatureAlgorithm];
static SUPPORTED_SIG_ALGS: SignatureAlgorithms = &[
    &webpki::ECDSA_P256_SHA256,
    &webpki::ECDSA_P256_SHA384,
    &webpki::ECDSA_P384_SHA256,
    &webpki::ECDSA_P384_SHA384,
    &webpki::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
    &webpki::RSA_PSS_2048_8192_SHA384_LEGACY_KEY,
    &webpki::RSA_PSS_2048_8192_SHA512_LEGACY_KEY,
    &webpki::RSA_PKCS1_2048_8192_SHA256,
    &webpki::RSA_PKCS1_2048_8192_SHA384,
    &webpki::RSA_PKCS1_2048_8192_SHA512,
    &webpki::RSA_PKCS1_3072_8192_SHA384,
];

// Do not confuse SgxEnclaveReport with AttestationReport.
// SgxReport is generated by SGX hardware and endorsed by Quoting Enclave through
// local attestation. The endorsed SgxReport is an SGX quote. The quote is then
// sent to some attestation service (IAS or DCAP-based AS). The endorsed SGX quote
// is an attestation report signed by attestation service private key, aka
// EndorsedAttestationReport
pub struct SgxEnclaveReport {
    pub cpu_svn: [u8; 16],
    pub misc_select: u32,
    pub attributes: [u8; 16],
    pub mr_enclave: [u8; 32],
    pub mr_signer: [u8; 32],
    pub isv_prod_id: u16,
    pub isv_svn: u16,
    pub report_data: [u8; 64],
}

impl std::fmt::Debug for SgxEnclaveReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "cpu_svn: {:?}", self.cpu_svn)?;
        writeln!(f, "misc_select: {:?}", self.misc_select)?;
        writeln!(f, "attributes: {:?}", self.attributes)?;
        writeln!(f, "mr_enclave: {:?}", self.mr_enclave)?;
        writeln!(f, "mr_signer: {:?}", self.mr_signer)?;
        writeln!(f, "isv_prod_id: {}", self.isv_prod_id)?;
        writeln!(f, "isv_svn: {}", self.isv_svn)?;
        writeln!(f, "report_data: {:?}", &self.report_data.to_vec())
    }
}

impl SgxEnclaveReport {
    pub fn parse_from<'a>(bytes: &'a [u8]) -> Result<Self> {
        let mut pos: usize = 0;
        let mut take = |n: usize| -> Result<&'a [u8]> {
            if n > 0 && bytes.len() >= pos + n {
                let ret = &bytes[pos..pos + n];
                pos += n;
                Ok(ret)
            } else {
                bail!("Quote parsing error.")
            }
        };

        // off 48, size 16
        let cpu_svn = <[u8; 16]>::try_from(take(16)?)?;

        // off 64, size 4
        let misc_select = u32::from_le_bytes(<[u8; 4]>::try_from(take(4)?)?);

        // off 68, size 28
        let _reserved = take(28)?;

        // off 96, size 16
        let attributes = <[u8; 16]>::try_from(take(16)?)?;

        // off 112, size 32
        let mr_enclave = <[u8; 32]>::try_from(take(32)?)?;

        // off 144, size 32
        let _reserved = take(32)?;

        // off 176, size 32
        let mr_signer = <[u8; 32]>::try_from(take(32)?)?;

        // off 208, size 96
        let _reserved = take(96)?;

        // off 304, size 2
        let isv_prod_id = u16::from_le_bytes(<[u8; 2]>::try_from(take(2)?)?);

        // off 306, size 2
        let isv_svn = u16::from_le_bytes(<[u8; 2]>::try_from(take(2)?)?);

        // off 308, size 60
        let _reserved = take(60)?;

        // off 368, size 64
        let mut report_data = [0u8; 64];
        let _report_data = take(64)?;
        let mut _it = _report_data.iter();
        for i in report_data.iter_mut() {
            *i = *_it.next().ok_or_else(|| anyhow!("Quote parsing error."))?;
        }

        ensure!(pos == bytes.len(), "Quote parsing error.");

        Ok(SgxEnclaveReport {
            cpu_svn,
            misc_select,
            attributes,
            mr_enclave,
            mr_signer,
            isv_prod_id,
            isv_svn,
            report_data,
        })
    }
}

#[derive(Debug, PartialEq)]
pub enum SgxQuoteVersion {
    V1(SgxEpidQuoteSigType),
    V2(SgxEpidQuoteSigType),
    V3(SgxEcdsaQuoteAkType),
}

#[derive(Debug, PartialEq)]
pub enum SgxEpidQuoteSigType {
    Unlinkable,
    Linkable,
}

#[derive(Debug, PartialEq)]
pub enum SgxEcdsaQuoteAkType {
    P256_256,
    P384_384,
}

#[derive(PartialEq, Debug)]
pub enum SgxQuoteStatus {
    OK,
    AttestationKeyRevoked,
    TcbOutOfDate,
    ConfigurationNeeded,
    TcbOutOfDateAndConfigurationNeeded,
    SignatureInvalid,
    SwHardeningNeeded,
    UnknownBadStatus,
}

impl From<&str> for SgxQuoteStatus {
    fn from(status: &str) -> Self {
        match status {
            "OK" => SgxQuoteStatus::OK,
            "KEY_REVOKED" => SgxQuoteStatus::AttestationKeyRevoked,
            "GROUP_OUT_OF_DATE" | "TCB_OUT_OF_DATE" => SgxQuoteStatus::TcbOutOfDate,
            "CONFIGURATION_NEEDED" => SgxQuoteStatus::ConfigurationNeeded,
            "OUT_OF_DATE_CONFIGURATION_NEEDED" => {
                SgxQuoteStatus::TcbOutOfDateAndConfigurationNeeded
            }
            "SIGNATURE_INVALID" => SgxQuoteStatus::SignatureInvalid,
            "SW_HARDENING_NEEDED" => SgxQuoteStatus::SwHardeningNeeded,
            _ => SgxQuoteStatus::UnknownBadStatus,
        }
    }
}

pub struct SgxQuote {
    pub version: SgxQuoteVersion,
    pub gid: u32,
    pub isv_svn_qe: u16,
    pub isv_svn_pce: u16,
    pub qe_vendor_id: Uuid,
    pub user_data: [u8; 20],
    pub isv_enclave_report: SgxEnclaveReport,
}

impl std::fmt::Debug for SgxQuote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "version: {:?}", self.version)?;
        writeln!(f, "gid: {}", self.gid)?;
        writeln!(f, "isv_svn_qe: {}", self.isv_svn_qe)?;
        writeln!(f, "isv_svn_pce: {}", self.isv_svn_pce)?;
        writeln!(f, "qe_vendor_id: {}", self.qe_vendor_id)?;
        writeln!(f, "user_data: {:?}", &self.user_data)?;
        writeln!(f, "isv_enclave_report: \n{:?}", self.isv_enclave_report)
    }
}

impl SgxQuote {
    fn parse_from<'a>(bytes: &'a [u8]) -> Result<Self> {
        let mut pos: usize = 0;
        let mut take = |n: usize| -> Result<&'a [u8]> {
            if n > 0 && bytes.len() >= pos + n {
                let ret = &bytes[pos..pos + n];
                pos += n;
                Ok(ret)
            } else {
                bail!("Quote parsing error.")
            }
        };

        // off 0, size 2 + 2
        let version = match u16::from_le_bytes(<[u8; 2]>::try_from(take(2)?)?) {
            1 => {
                let signature_type = match u16::from_le_bytes(<[u8; 2]>::try_from(take(2)?)?) {
                    0 => SgxEpidQuoteSigType::Unlinkable,
                    1 => SgxEpidQuoteSigType::Linkable,
                    _ => bail!("Quote parsing error."),
                };
                SgxQuoteVersion::V1(signature_type)
            }
            2 => {
                let signature_type = match u16::from_le_bytes(<[u8; 2]>::try_from(take(2)?)?) {
                    0 => SgxEpidQuoteSigType::Unlinkable,
                    1 => SgxEpidQuoteSigType::Linkable,
                    _ => bail!("Quote parsing error."),
                };
                SgxQuoteVersion::V2(signature_type)
            }
            3 => {
                let attestation_key_type = match u16::from_le_bytes(<[u8; 2]>::try_from(take(2)?)?)
                {
                    2 => SgxEcdsaQuoteAkType::P256_256,
                    3 => SgxEcdsaQuoteAkType::P384_384,
                    _ => bail!("Quote parsing error."),
                };
                SgxQuoteVersion::V3(attestation_key_type)
            }
            _ => bail!("Quote parsing error."),
        };

        // off 4, size 4
        let gid = u32::from_le_bytes(<[u8; 4]>::try_from(take(4)?)?);

        // off 8, size 2
        let isv_svn_qe = u16::from_le_bytes(<[u8; 2]>::try_from(take(2)?)?);

        // off 10, size 2
        let isv_svn_pce = u16::from_le_bytes(<[u8; 2]>::try_from(take(2)?)?);

        // off 12, size 16
        let qe_vendor_id_raw = <[u8; 16]>::try_from(take(16)?)?;
        let qe_vendor_id = Uuid::from_slice(&qe_vendor_id_raw)?;

        // off 28, size 20
        let user_data = <[u8; 20]>::try_from(take(20)?)?;

        // off 48, size 384
        let isv_enclave_report = SgxEnclaveReport::parse_from(take(384)?)?;

        ensure!(pos == bytes.len(), "Quote parsing error.");

        Ok(Self {
            version,
            gid,
            isv_svn_qe,
            isv_svn_pce,
            qe_vendor_id,
            user_data,
            isv_enclave_report,
        })
    }
}

#[derive(Debug)]
pub struct AttestationReport {
    pub freshness: Duration,
    pub sgx_quote_status: SgxQuoteStatus,
    pub sgx_quote_body: SgxQuote,
}

impl AttestationReport {
    pub fn from_cert(cert: &[u8], report_ca_cert: &[u8]) -> Result<Self> {
        // Before we reach here, Webpki already verifed the cert is properly signed
        use super::cert::*;

        let x509 = yasna::parse_der(cert, X509::load)?;

        let tbs_cert: <TbsCert as Asn1Ty>::ValueTy = x509.0;

        let pub_key: <PubKey as Asn1Ty>::ValueTy = ((((((tbs_cert.1).1).1).1).1).1).0;
        let pub_k = (pub_key.1).0;

        let sgx_ra_cert_ext: <SgxRaCertExt as Asn1Ty>::ValueTy =
            (((((((tbs_cert.1).1).1).1).1).1).1).0;

        let payload: Vec<u8> = ((sgx_ra_cert_ext.0).1).0;

        let report: EndorsedAttestationReport = serde_json::from_slice(&payload)?;
        let signing_cert = webpki::EndEntityCert::from(&report.signing_cert)?;

        let mut root_store = rustls::RootCertStore::empty();
        root_store
            .add(&rustls::Certificate(report_ca_cert.to_vec()))
            .expect("Failed to add CA");

        let trust_anchors: Vec<webpki::TrustAnchor> = root_store
            .roots
            .iter()
            .map(|cert| cert.to_trust_anchor())
            .collect();

        let chain: Vec<&[u8]> = vec![report_ca_cert];

        let time = webpki::Time::try_from(SystemTime::now())
            .map_err(|_| anyhow!("Cannot convert time."))?;

        signing_cert.verify_is_valid_tls_server_cert(
            SUPPORTED_SIG_ALGS,
            &webpki::TLSServerTrustAnchors(&trust_anchors),
            &chain,
            time,
        )?;

        // Verify the signature against the signing cert
        signing_cert.verify_signature(
            &webpki::RSA_PKCS1_2048_8192_SHA256,
            &report.report,
            &report.signature,
        )?;

        // Verify attestation report
        let attn_report: Value = serde_json::from_slice(&report.report)?;
        log::trace!("attn_report: {}", attn_report);

        // 1. Check timestamp is within 24H (90day is recommended by Intel)
        let quote_freshness = {
            let time = attn_report["timestamp"]
                .as_str()
                .ok_or_else(|| Error::new(AttestationError::ReportError))?;
            let time_fixed = String::from(time) + "+0000";
            let date_time = DateTime::parse_from_str(&time_fixed, "%Y-%m-%dT%H:%M:%S%.f%z")?;
            let ts = date_time.naive_utc();
            let now = DateTime::<chrono::offset::Utc>::from(SystemTime::now()).naive_utc();
            u64::try_from((now - ts).num_seconds())?
        };

        // 2. Get quote status
        let sgx_quote_status = {
            let status_string = attn_report["isvEnclaveQuoteStatus"]
                .as_str()
                .ok_or_else(|| Error::new(AttestationError::ReportError))?;

            SgxQuoteStatus::from(status_string)
        };

        // 3. Get quote body
        let sgx_quote_body = {
            let quote_encoded = attn_report["isvEnclaveQuoteBody"]
                .as_str()
                .ok_or_else(|| Error::new(AttestationError::ReportError))?;
            let quote_raw = base64::decode(&quote_encoded.as_bytes())?;
            SgxQuote::parse_from(quote_raw.as_slice())?
        };

        let raw_pub_k = pub_k.to_bytes();

        // According to RFC 5480 `Elliptic Curve Cryptography Subject Public Key Information',
        // SEC 2.2:
        // ``The first octet of the OCTET STRING indicates whether the key is
        // compressed or uncompressed.  The uncompressed form is indicated
        // by 0x04 and the compressed form is indicated by either 0x02 or
        // 0x03 (see 2.3.3 in [SEC1]).  The public key MUST be rejected if
        // any other value is included in the first octet.''
        //
        // We only accept the uncompressed form here.
        let is_uncompressed = raw_pub_k[0] == 4;
        let pub_k = &raw_pub_k.as_slice()[1..];
        if !is_uncompressed || pub_k != &sgx_quote_body.isv_enclave_report.report_data[..] {
            bail!(AttestationError::ReportError);
        }

        Ok(Self {
            freshness: std::time::Duration::from_secs(quote_freshness),
            sgx_quote_status,
            sgx_quote_body,
        })
    }
}

#[cfg(all(feature = "enclave_unit_test", feature = "mesalock_sgx"))]
pub mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read;
    use std::untrusted::fs::File;
    use teaclave_test_utils::*;

    fn tls_ra_cert_der() -> Vec<u8> {
        let mut cert = vec![];
        let mut f = File::open("fixtures/tls_ra_cert.der").unwrap();
        f.read_to_end(&mut cert).unwrap();

        cert
    }

    fn ias_root_ca_cert_der() -> Vec<u8> {
        let mut cert = vec![];
        let mut f = File::open("fixtures/ias_root_ca_cert.der").unwrap();
        f.read_to_end(&mut cert).unwrap();

        cert
    }

    fn attesation_report() -> Value {
        let report = json!({
            "version": 3,
            "timestamp": "2020-02-11T22:25:59.682915",
            "platformInfoBlob": "1502006504000900000D0D02040180030000000000000000000\
                                 A00000B000000020000000000000B2FE0AE0F7FD4D552BF7EF4\
                                 C938D44E349F1BD0E76F041362DC52B43B7B25994978D792137\
                                 90362F6DAE91797ACF5BD5072E45F9A60795D1FFB10140421D8\
                                 691FFD",
            "isvEnclaveQuoteStatus": "GROUP_OUT_OF_DATE",
            "isvEnclaveQuoteBody": "AgABAC8LAAAKAAkAAAAAAK1zRQOIpndiP4IhlnW2AkwAAAAA\
                                    AAAAAAAAAAAAAAAABQ4CBf+AAAAAAAAAAAAAAAAAAAAAAAAA\
                                    AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABwAAAAAAAAAHAAAA\
                                    AAAAADMKqRCjd2eA4gAmrj2sB68OWpMfhPH4MH27hZAvWGlT\
                                    AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACD1xnn\
                                    ferKFHD2uvYqTXdDA8iZ22kCD5xw7h38CMfOngAAAAAAAAAA\
                                    AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
                                    AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
                                    AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
                                    AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
                                    AAAAAAAAAADYIY9k0MVmCdIDUuFLf/2bGIHAfPjO9nvC7fgz\
                                    rQedeA3WW4dFeI6oe+RCLdV3XYD1n6lEZjITOzPPLWDxulGz",
            "id": "53530608302195762335736519878284384788",
            "epidPseudonym": "NRksaQej8R/SyyHpZXzQGNBXqfrzPy5KCxcmJrEjupXrq3xrm2y2+J\
                              p0IBVtcW15MCekYs9K3UH82fPyj6F5ciJoMsgEMEIvRR+csX9uyd54\
                              p+m+/RVyuGYhWbhUcpJigdI5Q3x04GG/A7EP10j/zypwqhYLQh0qN1\
                              ykYt1N1P0="
        });

        report
    }

    pub fn run_tests() -> bool {
        run_tests!(test_sgx_quote_parse_from, test_attestation_report_from_cert,)
    }

    fn test_sgx_quote_parse_from() {
        let attn_report = attesation_report();
        let sgx_quote_body_encoded = attn_report["isvEnclaveQuoteBody"].as_str().unwrap();
        let quote_raw = base64::decode(&sgx_quote_body_encoded.as_bytes()).unwrap();
        let sgx_quote = SgxQuote::parse_from(quote_raw.as_slice()).unwrap();

        assert_eq!(
            sgx_quote.version,
            SgxQuoteVersion::V2(SgxEpidQuoteSigType::Linkable)
        );
        assert_eq!(sgx_quote.gid, 2863);
        assert_eq!(sgx_quote.isv_svn_qe, 10);
        assert_eq!(sgx_quote.isv_svn_pce, 9);
        assert_eq!(
            sgx_quote.qe_vendor_id,
            Uuid::parse_str("00000000-ad73-4503-88a6-77623f822196").unwrap()
        );
        assert_eq!(
            sgx_quote.user_data,
            [117, 182, 2, 76, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );

        let isv_enclave_report = sgx_quote.isv_enclave_report;
        assert_eq!(
            isv_enclave_report.cpu_svn,
            [5, 14, 2, 5, 255, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(isv_enclave_report.misc_select, 0);
        assert_eq!(
            isv_enclave_report.attributes,
            [7, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            isv_enclave_report.mr_enclave,
            [
                51, 10, 169, 16, 163, 119, 103, 128, 226, 0, 38, 174, 61, 172, 7, 175, 14, 90, 147,
                31, 132, 241, 248, 48, 125, 187, 133, 144, 47, 88, 105, 83
            ]
        );
        assert_eq!(
            isv_enclave_report.mr_signer,
            [
                131, 215, 25, 231, 125, 234, 202, 20, 112, 246, 186, 246, 42, 77, 119, 67, 3, 200,
                153, 219, 105, 2, 15, 156, 112, 238, 29, 252, 8, 199, 206, 158
            ]
        );
        assert_eq!(isv_enclave_report.isv_prod_id, 0);
        assert_eq!(isv_enclave_report.isv_svn, 0);
        assert_eq!(
            isv_enclave_report.report_data.to_vec(),
            [
                216, 33, 143, 100, 208, 197, 102, 9, 210, 3, 82, 225, 75, 127, 253, 155, 24, 129,
                192, 124, 248, 206, 246, 123, 194, 237, 248, 51, 173, 7, 157, 120, 13, 214, 91,
                135, 69, 120, 142, 168, 123, 228, 66, 45, 213, 119, 93, 128, 245, 159, 169, 68,
                102, 50, 19, 59, 51, 207, 45, 96, 241, 186, 81, 179
            ]
            .to_vec()
        );
    }

    fn test_attestation_report_from_cert() {
        let tls_ra_cert = tls_ra_cert_der();
        let ias_root_ca_cert = ias_root_ca_cert_der();
        let report = AttestationReport::from_cert(&tls_ra_cert, &ias_root_ca_cert);
        assert!(report.is_ok());

        let report = report.unwrap();
        assert_eq!(report.sgx_quote_status, SgxQuoteStatus::TcbOutOfDate);
    }
}
