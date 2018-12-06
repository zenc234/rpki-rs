//! Identity Certificates.
//!

use bcder::{Mode, OctetString, Oid, Tag, Unsigned};
use bcder::{decode, encode};
use bcder::encode::Values;
use bcder::encode::Constructed;
use bytes::Bytes;
use cert::{SubjectPublicKeyInfo, Validity};
use cert::ext::{BasicCa, SubjectKeyIdentifier};
use cert::ext::oid;
use signing::SignatureAlgorithm;
use x509::{Name, SignedData, ValidationError};
use cert::ext::AuthorityKeyIdentifier;


//------------ IdCert --------------------------------------------------------

/// An Identity Certificate.
///
/// Identity Certificates are used in the provisioning and publication
/// protocol. Initially the parent and child CAs and/or the publishing CA
/// and publication server exchange self-signed Identity Certificates, wrapped
/// in XML messages defined in the 'exchange.rs' module.
///
/// The private keys corresponding to the subject public keys in these
/// certificates are then used to sign identity EE certificates used to sign
/// CMS messages in support of the provisioning and publication protocols.
///
/// NOTE: For the moment only V3 certificates are supported, because we insist
/// that a TA certificate is self-signed and has the CA bit set, and that an
/// EE certificate does not have this bit set, but does have an AKI that
/// matches the issuer's SKI. Maybe we should take this out... and just care
/// that things are validly signed, or only check AKI/SKI if it's version 3,
/// but skip this for lower versions.
#[derive(Clone, Debug)]
pub struct IdCert {
    /// The outer structure of the certificate.
    signed_data: SignedData,

    /// The serial number.
    serial_number: Unsigned,

    /// The algorithm used for signing the certificate.
    signature: SignatureAlgorithm,

    /// The name of the issuer.
    ///
    /// It isn’t really relevant in RPKI.
    issuer: Name,

    /// The validity of the certificate.
    validity: Validity,

    /// The name of the subject of this certificate.
    ///
    /// This isn’t really relevant in RPKI.
    subject: Name,

    /// Information about the public key of this certificate.
    subject_public_key_info: SubjectPublicKeyInfo,

    /// The certificate extensions.
    extensions: IdExtensions,
}

/// # Data Access
///
impl IdCert {
    /// Returns a reference to the certificate’s public key.
    pub fn public_key(&self) -> &[u8] {
        self.subject_public_key_info
            .subject_public_key().octet_slice().unwrap()
    }

    /// Returns a reference to the subject key identifier.
    pub fn subject_key_identifier(&self) -> &OctetString {
        &self.extensions.subject_key_id.subject_key_id()
    }

    /// Returns a reference to the entire public key information structure.
    pub fn subject_public_key_info(&self) -> &SubjectPublicKeyInfo {
        &self.subject_public_key_info
    }

    /// Returns a reference to the certificate’s serial number.
    pub fn serial_number(&self) -> &Unsigned {
        &self.serial_number
    }
}

/// # Decoding and Encoding
///
impl IdCert {
    /// Decodes a source as a certificate.
    pub fn decode<S: decode::Source>(source: S) -> Result<Self, S::Err> {
        Mode::Der.decode(source, Self::take_from)
    }

    /// Takes an encoded certificate from the beginning of a value.
    pub fn take_from<S: decode::Source>(
        cons: &mut decode::Constructed<S>
    ) -> Result<Self, S::Err> {
        cons.take_sequence(Self::from_constructed)
    }

    /// Parses the content of a Certificate sequence.
    pub fn from_constructed<S: decode::Source>(
        cons: &mut decode::Constructed<S>
    ) -> Result<Self, S::Err> {
        let signed_data = SignedData::from_constructed(cons)?;

        signed_data.data().clone().decode(|cons| {
            cons.take_sequence(|cons| {
                // version [0] EXPLICIT Version DEFAULT v1.
                //  -- we need extensions so apparently, we want v3 which,
                //     confusingly, is 2.
                cons.take_constructed_if(Tag::CTX_0, |c| c.skip_u8_if(2))?;

                Ok(IdCert {
                    signed_data,
                    serial_number: Unsigned::take_from(cons)?,
                    signature: SignatureAlgorithm::take_from(cons)?,
                    issuer: Name::take_from(cons)?,
                    validity: Validity::take_from(cons)?,
                    subject: Name::take_from(cons)?,
                    subject_public_key_info:
                    SubjectPublicKeyInfo::take_from(cons)?,
                    extensions: cons.take_constructed_if(
                        Tag::CTX_3,
                        IdExtensions::take_from
                    )?,
                })
            })
        }).map_err(Into::into)
    }

    pub fn encode<'a>(&'a self) -> impl encode::Values + 'a {
        self.signed_data.encode()
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut b = Vec::new();
        self.encode().write_encoded(Mode::Der, &mut b).unwrap(); // Writing to vec will not fail
        Bytes::from(b)
    }
}

/// # Validation
///
impl IdCert {
    /// Validates the certificate as a trust anchor.
    ///
    /// This validates that the certificate “is a current, self-signed RPKI
    /// CA certificate that conforms to the profile as specified in
    /// RFC6487” (RFC7730, section 3, step 2).
    pub fn validate_ta(self) -> Result<Self, ValidationError> {
        self.validate_basics()?;
        self.validate_ca_basics()?;

        // Authority Key Identifier. May be present, if so, must be
        // equal to the subject key identifier.
        if let Some(aki) = self.extensions.authority_key_id() {
            if aki != self.extensions.subject_key_id() {
                return Err(ValidationError);
            }
        }

        // Verify that this is self signed
        self.signed_data.verify_signature(
            self.subject_public_key_info
                .subject_public_key().octet_slice().unwrap()
        )?;

        Ok(self)
    }

    /// Validates the certificate as an EE certificate.
    ///
    /// For validation to succeed, the certificate needs to have been signed
    /// by the provided `issuer` certificate.
    ///
    /// Note that this does _not_ check the CRL.
    pub fn validate_ee(
        self,
        issuer: &IdCert,
    ) -> Result<Self, ValidationError> {
        self.validate_basics()?;
        self.validate_issued(issuer)?;

        // Basic Constraints: Must not be present.
        if self.extensions.basic_ca != None {
            return Err(ValidationError)
        }

        // Verify that this is signed by the issuer
        self.validate_signature(issuer)?;
        Ok(self)
    }


    //--- Validation Components

    /// Validates basic compliance with RFC8183 and RFC6492
    ///
    /// Note the the standards are pretty permissive in this context.
    fn validate_basics(&self) -> Result<(), ValidationError> {
        // Validity. Check according to RFC 5280.
        self.validity.validate()?;

        // Subject Key Identifer. Must be the SHA-1 hash of the octets
        // of the subjectPublicKey.
        if self.extensions.subject_key_id().as_slice().unwrap()
            != self.subject_public_key_info().key_identifier().as_ref()
        {
            return Err(ValidationError)
        }

        Ok(())
    }

    /// Validates that the certificate is a correctly issued certificate.
    ///
    /// Note this check is used to check that an EE certificate in an RFC8183,
    /// or RFC6492 message is validly signed by the TA certificate that was
    /// exchanged.
    ///
    /// This check assumes for now that we are always dealing with V3
    /// certificates and AKI and SKI have to match.
    fn validate_issued(
        &self,
        issuer: &IdCert,
    ) -> Result<(), ValidationError> {
        // Authority Key Identifier. Must be present and match the
        // subject key ID of `issuer`.
        if let Some(aki) = self.extensions.authority_key_id() {
            if aki != issuer.extensions.subject_key_id() {
                return Err(ValidationError)
            }
        }
        else {
            return Err(ValidationError);
        }

        Ok(())
    }

    /// Validates that the certificate is a valid CA certificate.
    ///
    /// Checks the parts that are common in normal and trust anchor CA
    /// certificates.
    fn validate_ca_basics(&self) -> Result<(), ValidationError> {
        // 4.8.1. Basic Constraints: For a CA it must be present (RFC6487)
        // und the “cA” flag must be set (RFC5280).
        if let Some(ref ca) = self.extensions.basic_ca {
            if ca.ca() == true {
                return  Ok(())
            }
        }

        Err(ValidationError)
    }

    /// Validates the certificate’s signature.
    fn validate_signature(
        &self,
        issuer: &IdCert
    ) -> Result<(), ValidationError> {
        self.signed_data.verify_signature(issuer.public_key())
    }
}


//--- AsRef

impl AsRef<IdCert> for IdCert {
    fn as_ref(&self) -> &Self {
        self
    }
}


//------------ IdExtensions --------------------------------------------------

#[derive(Clone, Debug)]
pub struct IdExtensions {
    /// Basic Constraints.
    ///
    /// The field indicates whether the extension is present and, if so,
    /// whether the "cA" boolean is set. See 4.8.1. of RFC 6487.
    basic_ca: Option<BasicCa>,

    /// Subject Key Identifier.
    subject_key_id: SubjectKeyIdentifier,

    /// Authority Key Identifier
    authority_key_id: Option<AuthorityKeyIdentifier>,
}

/// # Decoding
///
impl IdExtensions {
    pub fn take_from<S: decode::Source>(
        cons: &mut decode::Constructed<S>
    ) -> Result<Self, S::Err> {
        cons.take_sequence(|cons| {
            let mut basic_ca = None;
            let mut subject_key_id = None;
            let mut authority_key_id = None;
            while let Some(()) = cons.take_opt_sequence(|cons| {
                let id = Oid::take_from(cons)?;
                let critical = cons.take_opt_bool()?.unwrap_or(false);
                let value = OctetString::take_from(cons)?;
                Mode::Der.decode(value.to_source(), |content| {
                    if id == oid::CE_BASIC_CONSTRAINTS {
                        BasicCa::take(content, critical, &mut basic_ca)
                    } else if id == oid::CE_SUBJECT_KEY_IDENTIFIER {
                        SubjectKeyIdentifier::take(
                            content, critical, &mut subject_key_id
                        )
                    } else if id == oid::CE_AUTHORITY_KEY_IDENTIFIER {
                        AuthorityKeyIdentifier::take(
                            content, critical, &mut authority_key_id
                        )
                    } else if critical {
                        xerr!(Err(decode::Malformed))
                    } else {
                        // RFC 5280 says we can ignore non-critical
                        // extensions we don’t know of. RFC 6487
                        // agrees. So let’s do that.
                        Ok(())
                    }
                })?;
                Ok(())
            })? {}
            Ok(IdExtensions {
                basic_ca,
                subject_key_id: subject_key_id.ok_or(decode::Malformed)?,
                authority_key_id,
            })
        })
    }
}

/// # Encoding
///
// We have to do this the hard way because some extensions are optional.
// Therefore we need logic to determine which ones to encode.
impl IdExtensions {

    pub fn encode<'a>(&'a self) -> impl encode::Values + 'a {
        Constructed::new(
            Tag::CTX_3,
            encode::sequence(
                (
                    self.basic_ca.as_ref().map(|s| s.encode()),
                    self.subject_key_id.encode(),
                    self.authority_key_id.as_ref().map(|s| s.encode())
                )
            )
        )
    }

}


/// # Creating
///
impl IdExtensions {

    /// Creates extensions to be used on a self-signed TA IdCert
    pub fn for_id_ta_cert(key: &SubjectPublicKeyInfo) -> Self {
        IdExtensions{
            basic_ca: Some(BasicCa::new(true, true)),
            subject_key_id: SubjectKeyIdentifier::new(key),
            authority_key_id: Some(AuthorityKeyIdentifier::new(key))
        }
    }

    /// Creates extensions to be used on an EE IdCert in a protocol CMS
    pub fn for_id_ee_cert(
        subject_key: &SubjectPublicKeyInfo,
        authority_key: &SubjectPublicKeyInfo
    ) -> Self {
        IdExtensions{
            basic_ca: None,
            subject_key_id: SubjectKeyIdentifier::new(subject_key),
            authority_key_id: Some(AuthorityKeyIdentifier::new(authority_key))
        }
    }
}

/// # Data Access
///
impl IdExtensions {
    pub fn subject_key_id(&self) -> &OctetString {
        &self.subject_key_id.subject_key_id()
    }

    pub fn authority_key_id(&self) -> Option<&OctetString> {
        match &self.authority_key_id {
            Some(a) => Some(a.authority_key_id()),
            None => None
        }
    }
}


//------------ Tests ---------------------------------------------------------

// is pub so that we can use a parsed test IdCert for now for testing
#[cfg(test)]
pub mod tests {

    use super::*;
    use bytes::Bytes;

    use time;
    use chrono::{TimeZone, Utc};

    // Useful until we can create IdCerts of our own
    pub fn test_id_certificate() -> IdCert {
        let data = include_bytes!("../../test/oob/id-publisher-ta.cer");
        IdCert::decode(Bytes::from_static(data)).unwrap()
    }

    #[test]
    fn should_parse_id_publisher_ta_cert() {
        let d = Utc.ymd(2012, 1, 1).and_hms(0, 0, 0);
        time::with_now(d, || {
            let cert = test_id_certificate();
            assert!(cert.validate_ta().is_ok());
        });
    }

    #[test]
    fn should_encode_basic_ca() {
        let ba = BasicCa::new(true, true);
        let mut v = Vec::new();
        ba.encode().write_encoded(Mode::Der, &mut v).unwrap();

        // 48 15            Sequence with length 15
        //  6 3 85 29 19       OID 2.5.29.19 basicConstraints
        //  1 1 255              Boolean true
        //  4 5                OctetString of length 5
        //     48 3               Sequence with length 3
        //        1 1 255           Boolean true

        assert_eq!(
            vec![48, 15, 6, 3, 85, 29, 19, 1, 1, 255, 4, 5, 48, 3, 1, 1, 255 ],
            v
        );

    }
}