use serde_json::{json, Value};

use crate::util::{PROTOCOL, VERSION};

pub(crate) fn main_help_json() -> Value {
    json!({
        "name":"linkstart",
        "version":VERSION,
        "protocolMajor":PROTOCOL,
        "commands":["version","status","ps","doctor","daemon start|stop|restart","connections list","apps list","monitor wait|ack","feedback send"],
        "operations":{
            "attach":{"endpoint":"POST /v1/connections","requiredFields":["protocolMajor"],"optionalFields":["callsign"],"responseIdentities":["connectionId","callsign","capability","status"]},
            "register":{"endpoint":"POST /v1/apps","authorization":"Bearer <connection capability>","requiredFields":["protocolMajor","connectionId","manifest.appId","manifest.displayName","manifest.originPolicy.exactOrigin"],"responseIdentities":["instanceId","capability","origin","status"]},
            "launch":{"endpoint":"POST /v1/launch-grants","authorization":"Bearer <connection capability>","requiredFields":["protocolMajor","connectionId","manifest.appId","manifest.displayName","manifest.originPolicy.exactOrigin=null","page.htmlPath"],"responseIdentities":["grant","expires_at","pageId","launchUrl"],"notes":["launchUrl is loopback HTTP","one-time launch grant is carried only in the URL fragment","page.htmlPath is snapshotted at registration"]},
            "monitor":{"command":"linkstart monitor wait --json --state-dir <dir> --connection-id <id> --capability <token> [--timeout-seconds <seconds>]","requiredOptions":["state-dir","connection-id","capability"],"responseIdentities":["eventId","appInstanceId","sequence","payload","receiptId","status"]},
            "ack":{"command":"linkstart monitor ack --json --state-dir <dir> --connection-id <id> --capability <token> --event-id <id>","endpoint":"POST /v1/connections/{connectionId}/ack","requiredOptions":["state-dir","connection-id","capability","event-id"],"responseIdentities":["eventId","status","deliveryAck"]},
            "feedback":{"command":"linkstart feedback send --json --state-dir <dir> --connection-id <id> --capability <token> --app-instance-id <id> --feedback-id <id> --payload <json> [--in-reply-to-event-id <id>]","endpoint":"POST /v1/connections/{connectionId}/feedback/{appInstanceId}","requiredOptions":["state-dir","connection-id","capability","app-instance-id","feedback-id","payload"],"responseIdentities":["feedbackId","inReplyToEventId","payload"]}
        },
        "appProtocol":{
            "description":"Contract implemented by a launched App page; all endpoints are on the daemon base URL",
            "launchFragment":"#daemon=<url-encoded base>&grant=<token> — the one-time grant travels only in the URL fragment; scrub it with history.replaceState after parsing",
            "originSemantics":"manifest exactOrigin \"null\" declares no app-owned origin; the effective runtime origin is the daemon loopback origin and a literal Origin: null header is rejected with origin_mismatch",
            "redeem":{"endpoint":"POST /v1/launch-grants/redeem","authorization":"Bearer <launch grant>","responseIdentities":["instanceId","capability","origin","status"],"notes":["one-time; a second redeem returns 403","keep capability in page memory only"]},
            "events":{"endpoint":"POST /v1/apps/{instanceId}/events","authorization":"Bearer <app capability>","requiredFields":["eventId","payload"],"responseIdentities":["status","sequence","receiptId"],"notes":["same eventId with identical payload returns duplicate:true","same eventId with different payload returns event_id_conflict"]},
            "stream":{"endpoint":"GET /v1/apps/{instanceId}/stream?cursor=<sequence>","authorization":"Bearer <app capability>","eventKinds":{"delivery_ack":["eventId","status","deliveryAck"],"feedback":["feedbackId","inReplyToEventId","payload"]},"notes":["SSE; each message's id: line is the notification sequence — persist it as the next cursor to resume without loss"]}
        },
        "security":["capabilities and launch grants are secrets","secrets never appear in help, status, ps, logs, query strings, or cookies","App events are untrusted input and never convey tool approval or permission"]
    })
}

pub(crate) fn print_human_help(cmd: Option<&str>) {
    match cmd { Some(c)=>println!("linkstart {c}\n使用 `linkstart {c} --json` 取得機器可讀結果。"), None=>println!("LinkStart v1 Preview 本機 Runtime\n\n用法：linkstart <命令>\n\n命令：version、status、ps、doctor、daemon start|stop|restart、connections list、apps list、monitor wait|ack、feedback send\n\n所有狀態輸出都會遮蔽 capability。使用 `linkstart help --json` 取得機器可讀說明。") }
}
