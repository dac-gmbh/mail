use std::ops::Deref;
use std::fmt;

use mime::{Mime, BOUNDARY};

use codec::{EncodableInHeader, Encoder, Encodable};
use soft_ascii_string::SoftAsciiString;
use futures::{ Future, Async, Poll };

use error::*;
use utils::HeaderTryInto;
use headers::{
    Header, HeaderMap,
    ContentType, From,
    ContentTransferEncoding,
    Date, MessageId
};
use components::DateTime;

use self::builder::{
    check_header,
    check_multiple_headers,
};
use self::mime::write_random_boundary_to;

pub use self::builder::{
    Builder, MultipartBuilder, SinglepartBuilder
};
pub use self::context::*;
pub use self::resource::*;


pub mod mime;
mod resource;
mod builder;
mod encode;
mod context;



pub struct Mail {
    //NOTE: by using some OwnedOrStaticRef AsciiStr we can probably safe a lot of
    // unnecessary allocations
    headers: HeaderMap,
    body: MailPart,
}


pub enum MailPart {
    SingleBody {
        body: Resource
    },
    MultipleBodies {
        bodies: Vec<Mail>,
        hidden_text: SoftAsciiString
    }
}

/// a future resolving to an encodeable mail
///
/// The future resolves like this:
///
/// 1. it makes sure all contained futures are resolved, i.e. all
///    `Resources` are loaded and transfer encoded if needed
/// 2. it inserts auto-generated headers, i.e. `Content-Type`,
///    `Content-Transfer-Encoding` are generated and `Date`, too if
///    it is needed.
///     1. If needed it generates a boundary for the `Content-Type`
/// 3. contextual validators are used (including a check if there is
///    a `From` header)
/// 4. as the mail is now ready to be encoded it resolves to an
///    `EncodableMail`
///
/// # Error (while resolving the future)
///
/// - if one of the contained futures fails, e.g. if a resource can not
///   be loaded or encoded
/// - if a contextual validator fails, e.g. `From` header is missing or
///   there is a multi mailbox `From` header but no `Sender` header
///
pub struct MailFuture<'a, T: 'a> {
    mail: Option<Mail>,
    ctx: &'a T
}

/// a mail with all contained futures resolved, so that it can be encoded
pub struct EncodableMail( Mail );

impl Mail {


    /// adds a new header,
    ///
    /// - if the header already existed, the existing one will be overriden and the
    ///   old header will be returned
    /// - `Content-Transfer-Encoding` it might be overwritten later one
    ///
    /// # Failure
    ///
    /// if a Content-Type header is set, which conflicts with the body, mainly if
    /// you set a multipart content type on a non-multipart body or the other way around
    ///
    pub fn set_header<H, C>( &mut self, header: H, comp: C) -> Result<()>
        where H: Header,
              H::Component: EncodableInHeader,
              C: HeaderTryInto<H::Component>
    {
        let comp = comp.try_into()?;
        check_header::<H>( &comp, self.body.is_multipart() )?;
        self.headers.insert( header, comp )?;
        Ok( () )
    }

    pub fn set_headers( &mut self, headers: HeaderMap ) -> Result<()> {
        check_multiple_headers( &headers, self.body.is_multipart() )?;
        self.headers.extend( headers )?;
        Ok( () )
    }

    pub fn headers( &self ) -> &HeaderMap {
        &self.headers
    }

    pub fn body( &self ) -> &MailPart {
        &self.body
    }

    /// Turns the mail into a future with resolves to an `EncodeableMail`
    ///
    pub fn into_encodeable_mail<'a, C: BuilderContext>(self, ctx: &'a C ) -> MailFuture<'a, C> {
        MailFuture {
            ctx,
            mail: Some( self )
        }
    }

    fn walk_mail_bodies_mut<FN>( &mut self, use_it_fn: &mut FN) -> Result<()>
        where FN: FnMut( &mut Resource ) -> Result<()>
    {
        use self::MailPart::*;
        match self.body {
            SingleBody { ref mut body } =>
                use_it_fn( body )?,
            MultipleBodies { ref mut bodies, .. } =>
                for body in bodies {
                    body.walk_mail_bodies_mut( use_it_fn )?
                }
        }
        Ok( () )
    }
}







impl MailPart {

    pub fn is_multipart( &self ) -> bool {
        use self::MailPart::*;
        match *self {
            SingleBody { .. } => false,
            MultipleBodies { .. } => true
        }
    }
}


impl<'a, T> Future for MailFuture<'a, T>
    where T: BuilderContext,
{
    type Item = EncodableMail;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut done = true;
        let ctx: &T = &self.ctx;
        self.mail.as_mut()
            // this is conform with how futures work, as calling poll on a random future
            // after it completes has unpredictable results (through one of NotReady/Err/Panic)
            // use `Fuse` if you want more preditable behaviour in this edge case
            .expect( "poll not to be called after completion" )
            .walk_mail_bodies_mut( &mut |body: &mut Resource| {
                match body.poll_encoding_completion( ctx ) {
                    Ok( Async::NotReady ) => {
                        done = false;
                        Ok(())
                    },
                    Ok( Async::Ready( .. ) ) => {
                        Ok(())
                    },
                    Err( err ) => {
                         Err( err )
                    }
                }
            })?;

        if done {
            EncodableMail::from_loaded_mail( self.mail.take().unwrap() )
                .map( |enc_mail| Async::Ready(enc_mail) )
        } else {
            Ok( Async::NotReady )
        }
    }
}

impl EncodableMail {

    fn from_loaded_mail(mut mail: Mail) -> Result<Self> {
        insert_generated_headers(&mut mail)?;
        // also insert `Date` if needed, but only on the outer most header map
        if !mail.headers.contains(Date) {
            mail.headers.insert(Date, DateTime::now())?;
        }

        mail.headers.use_contextual_validators()?;

        // also check `From` only on the outer most header map
        if !mail.headers.contains(From) {
            bail!("mail must have a `From` header");
        }
        if !mail.headers.contains(MessageId) {
            warn!("mail without MessageId")
        }

        Ok(EncodableMail(mail))
    }
}

/// inserts ContentType and ContentTransferEncoding into
/// the headers of any contained `MailPart::SingleBody`,
/// based on the `Resource` representing the body
fn insert_generated_headers(mail: &mut Mail) -> Result<()> {
    match mail.body {
        MailPart::SingleBody { ref body } => {
           auto_gen_headers(&mut mail.headers, body)?;
        }
        MailPart::MultipleBodies { ref mut bodies, .. } => {
            auto_gen_multipart(&mut mail.headers)?;
            for sub_mail in bodies {
                insert_generated_headers(sub_mail)?;
            }
        }

    }
    Ok(())
}

fn auto_gen_multipart(headers: &mut HeaderMap) -> Result<()> {
    // ask following: <has content type header?><is correct type?><needs new mime?>
    let current: Option<Result<Option<Mime>>> = headers.get_single(ContentType)
        .map(|res| {
            let content_type = res?;
            if content_type.get_param(BOUNDARY).is_none() {
                debug_assert_eq!(content_type.type_(), "multipart");
                let mut boundary = String::new();
                write_random_boundary_to(&mut boundary);
                let mime_string = format!("{};boundary={}", content_type, boundary);
                let new_content_type: Mime = mime_string.parse()
                    .chain_err( || "[BUG] mime invalidated by adding boundary" )?;
                Ok(Some(new_content_type))
            } else {
                Ok(None)
            }
        });
    if let Some(res) = current {
        let maybe_new_content_type = res?;
        if let Some(content_type) = maybe_new_content_type {
            //FIXME uhm, get_single_mut?, entry?
            let had_content_type = headers.remove_by_name(ContentType);
            debug_assert!(had_content_type);
            headers.insert(ContentType, content_type)?;
        }
    } else {
        bail!("[BUG] multipart does not have content type header, should be enforce by builder")
    }

    // we do not care at this point that it's missing
    // checks for quantities are doe after auto generation
    Ok(())
}

fn auto_gen_headers(headers: &mut HeaderMap, body: &Resource) -> Result<()> {
    let file_buffer = body.get_if_encoded()?
        .expect("encoded mail, should only contain already transferencoded resources");

    headers.insert(ContentType, file_buffer.content_type().clone())?;
    headers.insert(ContentTransferEncoding, file_buffer.transfer_encoding().clone())?;
    Ok(())
}

impl Deref for EncodableMail {

    type Target = Mail;
    fn deref( &self ) -> &Self::Target {
        &self.0
    }
}

impl Into<Mail> for EncodableMail {
    fn into( self ) -> Mail {
        self.0
    }
}

impl Encodable<Resource> for EncodableMail {

    fn encode(&self, encoder:  &mut Encoder<Resource>) -> Result<()> {
        // does not panic as a EncodableMail only is constructed from
        // a Mail which has all of it's bodies resolved, without failure
        encode::encode_mail( &self, true, encoder )
    }
}

impl fmt::Debug for EncodableMail {
    fn fmt(&self, fter: &mut fmt::Formatter) -> fmt::Result {
        write!(fter, "EncodableMail {{ .. }}")
    }
}


#[cfg(test)]
mod test {

    mod Mail {
        #![allow(non_snake_case)]
        use super::super::*;
        use headers::{
            Subject, Comments
        };
        use futures::future;
        use std::str;

        fn load_blocking<C>(r: &mut Resource, ctx: &C)
            where C: BuilderContext
        {
            let fut = future::poll_fn(|| r.poll_encoding_completion(ctx));
            fut.wait().unwrap();
        }

        #[test]
        fn walk_mail_bodies_does_not_skip() {
            let mut mail = Mail {
                headers: HeaderMap::new(),
                body: MailPart::MultipleBodies {
                    bodies: vec! [
                        Mail {
                            headers: HeaderMap::new(),
                            body: MailPart::MultipleBodies {
                                bodies: vec! [
                                    Mail {
                                        headers: HeaderMap::new(),
                                        body: MailPart::SingleBody {
                                            body: Resource::from_text("r1".into())
                                        }
                                    },
                                    Mail {
                                        headers: HeaderMap::new(),
                                        body: MailPart::SingleBody {
                                            body: Resource::from_text("r2".into())
                                        }
                                    }
                                ],
                                hidden_text: Default::default()
                            }
                        },
                        Mail {
                            headers: HeaderMap::new(),
                            body: MailPart::SingleBody {
                                body: Resource::from_text("r3".into())
                            }
                        }

                    ],
                    hidden_text: Default::default()
                }
            };

            let ctx = ::default_impl::SimpleBuilderContext::new();
            let mut body_count = 0;
            mail.walk_mail_bodies_mut(&mut |body: &mut Resource| {
                body_count += 1;
                load_blocking(body, &ctx);
                let encoded = assert_ok!(body.get_if_encoded());
                let encoded = encoded.expect("it should be loaded");
                let slice = str::from_utf8(&encoded[..]).unwrap();
                assert!([ "r1", "r2", "r3"].contains(&slice));
                Ok(())
            }).unwrap();
        }

        #[test]
        fn walk_mail_bodies_handles_errors() {
            let mut mail = Mail {
                headers: HeaderMap::new(),
                body: MailPart::SingleBody {
                    body: Resource::from_text("r0".into()),
                }
            };
            assert_ok!(mail.walk_mail_bodies_mut(&mut |_| { Ok(()) }));
            assert_err!(mail.walk_mail_bodies_mut(&mut |_| { bail!("bad") }));
        }

        #[test]
        fn set_header_checks_the_header() {
            let mut mail = Mail {
                headers: HeaderMap::new(),
                body: MailPart::SingleBody {
                    body: Resource::from_text("r0".into()),
                }
            };

            assert_err!(
                mail.set_header(ContentTransferEncoding, ::components::TransferEncoding::Base64));
            //Note: a more fine grained test is done in ::mail::builder::test
            assert_err!(mail.set_header(ContentType, "text/plain"));
            assert_err!(mail.set_header(ContentType, "multipart/plain"));
        }

        #[test]
        fn set_header_set_a_header() {
            let mut mail = Mail {
                headers: HeaderMap::new(),
                body: MailPart::SingleBody {
                    body: Resource::from_text("r0".into()),
                }
            };
            assert_ok!(mail.set_header(Subject, "hy"));
            assert!(mail.headers().contains(Subject));
        }

        #[test]
        fn set_headers_checks_the_headers() {
            let mut mail = Mail {
                headers: HeaderMap::new(),
                body: MailPart::SingleBody {
                    body: Resource::from_text("r0".into()),
                }
            };
            assert_err!(mail.set_headers(headers! {
                ContentType: "test/html;charset=utf8"
            }.unwrap()));
        }

        #[test]
        fn set_headers_sets_all_headers() {
            let mut mail = Mail {
                headers: HeaderMap::new(),
                body: MailPart::SingleBody {
                    body: Resource::from_text("r0".into()),
                }
            };
            assert_ok!(mail.set_headers(headers! {
                Subject: "yes",
                Comments: "so much"
            }.unwrap()));

            assert!(mail.headers().contains(Subject));
            assert!(mail.headers().contains(Comments));
        }

    }

    mod EncodableMail {
        #![allow(non_snake_case)]
        use super::super::*;
        use chrono::{Utc, TimeZone};
        use components::{
            TransferEncoding,
            DateTime
        };
        use headers::{
            From, ContentType, ContentTransferEncoding,
            Date, Subject
        };

        #[test]
        fn sets_generated_headers_for_outer_mail() {
            let mut resource = Resource::from_text("r9".into());
            resource.set_preferred_encoding(TransferEncoding::Base64);
            let mail = Mail {
                headers: headers!{
                    From: ["random@this.is.no.mail"],
                    Subject: "hoho"
                }.unwrap(),
                body: MailPart::SingleBody { body: resource }
            };

            let ctx = ::default_impl::SimpleBuilderContext::new();
            let enc_mail = assert_ok!(mail.into_encodeable_mail(&ctx).wait());

            let headers: &HeaderMap = enc_mail.headers();
            assert!(headers.contains(From));
            assert!(headers.contains(Subject));
            assert!(headers.contains(Date));
            assert!(headers.contains(ContentType));
            assert!(headers.contains(ContentTransferEncoding));

            let res = headers.get_single(ContentType)
                .unwrap()
                .unwrap();

            assert_eq!(res, &"text/plain;charset=utf8");

            let res = headers.get_single(ContentTransferEncoding)
                .unwrap()
                .unwrap();

            assert_eq!(res, &TransferEncoding::Base64);
        }

        #[test]
        fn sets_generated_headers_for_sub_mails() {
            let mut resource = Resource::from_text("r9".into());
            resource.set_preferred_encoding(TransferEncoding::Base64);
            let mail = Mail {
                headers: headers!{
                    From: ["random@this.is.no.mail"],
                    Subject: "hoho",
                    ContentType: "multipart/mixed"
                }.unwrap(),
                body: MailPart::MultipleBodies {
                    bodies: vec![
                        Mail {
                            headers: HeaderMap::new(),
                            body: MailPart::SingleBody { body: resource }
                        }
                    ],
                    hidden_text: Default::default()
                }
            };

            let ctx = ::default_impl::SimpleBuilderContext::new();
            let mail = mail.into_encodeable_mail(&ctx).wait().unwrap();

            assert!(mail.headers().contains(From));
            assert!(mail.headers().contains(Subject));
            assert!(mail.headers().contains(Date));
            //the Builder would have set it but as we didn't use it (intentionally) it's not set
            //assert!(headers.contains(ContentType));

            if let MailPart::MultipleBodies { ref bodies, ..} = mail.body {
                let headers = bodies[0].headers();
                assert_not!(headers.contains(Date));

                let res = headers.get_single(ContentType)
                    .unwrap()
                    .unwrap();

                assert_eq!(res, &"text/plain;charset=utf8");

                let res = headers.get_single(ContentTransferEncoding)
                    .unwrap()
                    .unwrap();

                assert_eq!(res, &TransferEncoding::Base64);

            } else {
                unreachable!()
            }
        }

        #[test]
        fn runs_contextual_validators() {
            let mail = Mail {
                headers: headers!{
                    From: ["random@this.is.no.mail", "u.p.s@s.p.u"],
                    Subject: "hoho"
                }.unwrap(),
                body: MailPart::SingleBody { body: Resource::from_text("r9".into()) }
            };

            let ctx = ::default_impl::SimpleBuilderContext::new();
            assert_err!(mail.into_encodeable_mail(&ctx).wait());
        }

        #[test]
        fn checks_there_is_from() {
            let mail = Mail {
                headers: headers!{
                    Subject: "hoho"
                }.unwrap(),
                body: MailPart::SingleBody { body: Resource::from_text("r9".into()) }
            };

            let ctx = ::default_impl::SimpleBuilderContext::new();
            assert_err!(mail.into_encodeable_mail(&ctx).wait());
        }

        #[test]
        fn does_not_override_date_if_set() {
            let provided_date = Utc.ymd(1992, 5, 25).and_hms(23, 41, 12);
            let mail = Mail {
                headers: headers!{
                    From: ["random@this.is.no.mail"],
                    Subject: "hoho",
                    Date: DateTime::new(provided_date.clone())
                }.unwrap(),
                body: MailPart::SingleBody { body: Resource::from_text("r9".into()) }
            };

            let ctx = ::default_impl::SimpleBuilderContext::new();
            let enc_mail = assert_ok!(mail.into_encodeable_mail(&ctx).wait());
            let used_date = enc_mail.headers()
                .get_single(Date)
                .unwrap()
                .unwrap();

            assert_eq!(**used_date, provided_date);


        }

        #[test]
        fn generates_boundary_if_needed() {
            let mail = Mail {
                headers: headers!{
                    From: ["random@this.is.no.mail"],
                    ContentType: "multipart/mixed"
                }.unwrap(),
                body: MailPart::MultipleBodies {
                    bodies: vec![
                        Mail {
                            headers: headers!{
                                From: ["random@this.is.no.mail"]
                            }.unwrap(),
                            body: MailPart::SingleBody {
                                body: Resource::from_text("r9".into())
                            }
                        }
                    ],
                    hidden_text: Default::default()
                }
            };

            let ctx = ::default_impl::SimpleBuilderContext::new();
            let enc_mail = assert_ok!(mail.into_encodeable_mail(&ctx).wait());


            let headers = enc_mail.headers();
            let ct = headers.get_single(ContentType)
                .expect("there has to be a ContentType header");
            let ct = assert_ok!(ct);
            let pm = format!("no boundary on: {}", ct);
            assert!(ct.get_param(::mime::BOUNDARY).is_some(), pm);
        }
    }

}