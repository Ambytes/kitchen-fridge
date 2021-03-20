//! This module provides a client to connect to a CalDAV server

use std::error::Error;
use std::convert::TryFrom;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use reqwest::Method;
use reqwest::header::CONTENT_TYPE;
use minidom::Element;
use url::Url;

use crate::utils::{find_elem, find_elems};
use crate::calendar::remote_calendar::RemoteCalendar;
use crate::calendar::CalendarId;
use crate::traits::CalDavSource;
use crate::traits::PartialCalendar;


static DAVCLIENT_BODY: &str = r#"
    <d:propfind xmlns:d="DAV:">
       <d:prop>
           <d:current-user-principal />
       </d:prop>
    </d:propfind>
"#;

static HOMESET_BODY: &str = r#"
    <d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" >
      <d:self/>
      <d:prop>
        <c:calendar-home-set />
      </d:prop>
    </d:propfind>
"#;

static CAL_BODY: &str = r#"
    <d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" >
       <d:prop>
         <d:displayname />
         <d:resourcetype />
         <c:supported-calendar-component-set />
       </d:prop>
    </d:propfind>
"#;



/// A CalDAV source that fetches its data from a CalDAV server
pub struct Client {
    url: Url,
    username: String,
    password: String,

    /// The interior mutable part of a Client.
    /// This data may be retrieved once and then cached
    cached_replies: Mutex<CachedReplies>,
}


#[derive(Default)]
struct CachedReplies {
    principal: Option<Url>,
    calendar_home_set: Option<Url>,
    calendars: Option<HashMap<CalendarId, Arc<Mutex<RemoteCalendar>>>>,
}

impl Client {
    /// Create a client. This does not start a connection
    pub fn new<S: AsRef<str>, T: ToString, U: ToString>(url: S, username: T, password: U) -> Result<Self, Box<dyn Error>> {
        let url = Url::parse(url.as_ref())?;

        Ok(Self{
            url,
            username: username.to_string(),
            password: password.to_string(),
            cached_replies: Mutex::new(CachedReplies::default()),
        })
    }

    async fn sub_request(&self, url: &Url, body: String, depth: u32) -> Result<String, Box<dyn Error>> {
        let method = Method::from_bytes(b"PROPFIND")
            .expect("cannot create PROPFIND method.");

        let res = reqwest::Client::new()
            .request(method, url.as_str())
            .header("Depth", depth)
            .header(CONTENT_TYPE, "application/xml")
            .basic_auth(self.username.clone(), Some(self.password.clone()))
            .body(body)
            .send()
            .await?;
        let text = res.text().await?;
        Ok(text)
    }

    async fn sub_request_and_process(&self, url: &Url, body: String, items: &[&str]) -> Result<String, Box<dyn Error>> {
        let text = self.sub_request(url, body, 0).await?;

        let mut current_element: &Element = &text.parse().unwrap();
        for item in items {
                current_element = find_elem(&current_element, item).unwrap();
        }
        Ok(current_element.text())
    }

    /// Return the Principal URL, or fetch it from server if not known yet
    async fn get_principal(&self) -> Result<Url, Box<dyn Error>> {
        if let Some(p) = &self.cached_replies.lock().unwrap().principal {
            return Ok(p.clone());
        }

        let href = self.sub_request_and_process(&self.url, DAVCLIENT_BODY.into(), &["current-user-principal", "href"]).await?;
        let mut principal_url = self.url.clone();
        principal_url.set_path(&href);
        self.cached_replies.lock().unwrap().principal = Some(principal_url.clone());
        log::debug!("Principal URL is {}", href);

        return Ok(principal_url);
    }

    /// Return the Homeset URL, or fetch it from server if not known yet
    async fn get_cal_home_set(&self) -> Result<Url, Box<dyn Error>> {
        if let Some(h) = &self.cached_replies.lock().unwrap().calendar_home_set {
            return Ok(h.clone());
        }
        let principal_url = self.get_principal().await?;

        let href = self.sub_request_and_process(&principal_url, HOMESET_BODY.into(), &["calendar-home-set", "href"]).await?;
        let mut chs_url = self.url.clone();
        chs_url.set_path(&href);
        self.cached_replies.lock().unwrap().calendar_home_set = Some(chs_url.clone());
        log::debug!("Calendar home set URL is {:?}", chs_url.path());

        Ok(chs_url)
    }

    async fn populate_calendars(&self) -> Result<(), Box<dyn Error>> {
        let cal_home_set = self.get_cal_home_set().await?;

        let text = self.sub_request(&cal_home_set, CAL_BODY.into(), 1).await?;

        let root: Element = text.parse().unwrap();
        let reps = find_elems(&root, "response");
        let mut calendars = HashMap::new();
        for rep in reps {
            let display_name = find_elem(rep, "displayname").map(|e| e.text()).unwrap_or("<no name>".to_string());
            log::debug!("Considering calendar {}", display_name);

            // We filter out non-calendar items
            let resource_types = match find_elem(rep, "resourcetype") {
                None => continue,
                Some(rt) => rt,
            };
            let mut found_calendar_type = false;
            for resource_type in resource_types.children() {
                if resource_type.name() == "calendar" {
                    found_calendar_type = true;
                    break;
                }
            }
            if found_calendar_type == false {
                continue;
            }

            // We filter out the root calendar collection, that has an empty supported-calendar-component-set
            let el_supported_comps = match find_elem(rep, "supported-calendar-component-set") {
                None => continue,
                Some(comps) => comps,
            };
            if el_supported_comps.children().count() == 0 {
                continue;
            }

            let calendar_href = match find_elem(rep, "href") {
                None => {
                    log::warn!("Calendar {} has no URL! Ignoring it.", display_name);
                    continue;
                },
                Some(h) => h.text(),
            };

            let mut this_calendar_url = self.url.clone();
            this_calendar_url.set_path(&calendar_href);

            let supported_components = match crate::calendar::SupportedComponents::try_from(el_supported_comps.clone()) {
                Err(err) => {
                    log::warn!("Calendar {} has invalid supported components ({})! Ignoring it.", display_name, err);
                    continue;
                },
                Ok(sc) => sc,
            };
            let this_calendar = RemoteCalendar::new(display_name, this_calendar_url, supported_components);
            log::info!("Found calendar {}", this_calendar.name());
            calendars.insert(this_calendar.id().clone(), Arc::new(Mutex::new(this_calendar)));
        }

        let mut replies = self.cached_replies.lock().unwrap();
        replies.calendars = Some(calendars);
        Ok(())
    }

}

#[async_trait]
impl CalDavSource<RemoteCalendar> for Client {
    async fn get_calendars(&self) -> Result<HashMap<CalendarId, Arc<Mutex<RemoteCalendar>>>, Box<dyn Error>> {
        self.populate_calendars().await?;

        match &self.cached_replies.lock().unwrap().calendars {
            Some(cals) => {
                return Ok(cals.clone())
            },
            None => return Err("No calendars available".into())
        };
    }


    async fn get_calendar(&self, id: CalendarId) -> Option<Arc<Mutex<RemoteCalendar>>> {
        self.cached_replies.lock().unwrap()
            .calendars
            .as_ref()
            .and_then(|cals| cals.get(&id))
            .map(|cal| cal.clone())
        }
}

