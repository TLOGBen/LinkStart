use serde_json::{json, Value};

use crate::util::{PROTOCOL, VERSION};

pub(crate) fn main_help_json() -> Value {
    json!({
        "name":"linkstart",
        "version":VERSION,
        "protocolMajor":PROTOCOL,
        "commands":["version","status","ps","doctor","daemon start|stop|restart","connections list","apps list","monitor wait|ack","feedback send","tool"],
        "runtimeV2":{"console":"http://<daemon-address>/console","toolCommand":"linkstart tool --state-dir <dir> --credential-file <path> < request.json","request":{"request_id":"stable caller request id","tool":"spaces.list","args":{}},"agentRecovery":{"request":{"request_id":"explicit-recovery-attempt-id","tool":"agent.recover","space_id":"original-space-id","args":{"job_id":"existing-job-id","mission_topic_id":"original-mission-id"}},"notes":["Same currently authorized task_decomposer identity only; credential rotation is supported","Reuses the durable saved plan and stable child requests; never calls the model","Keep the recovery request_id after an unknown transport result; first inspect agent.get with the same space, job_id and mission_topic_id","A new explicit request_id is required for a new recovery attempt; agent.run and agent.get never resume work","This is not provider-native conversation resume, history replay or terminal.restart","Stop all old daemon/workers before upgrading; ownership assumes a local canonical state directory, not a network filesystem"]},"notes":["JSON request is read from stdin; secrets are read from a protected file, never argv","GUI and CLI share POST /v2/tools and server authorization","native terminal is raw bytes, not a structured Agent SDK","unconfirmed native effects are never automatically retried"]},
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
    match cmd {
        Some("tool") => println!("linkstart tool --state-dir <目錄> --credential-file <憑證檔> < request.json\n從 stdin 讀取 JSON 工具請求，透過 Runtime HTTP 執行並回傳 JSON。憑證由檔案讀取，不放在命令參數。\n請求範例：{{\"request_id\":\"read-spaces-1\",\"tool\":\"spaces.list\",\"args\":{{}}}}"),
        Some(c) => println!("linkstart {c}\n使用 `linkstart {c} --json` 取得機器可讀結果。"),
        None => println!("LinkStart 本機 Runtime\n\n用法：linkstart <命令>\n\n命令：version、status、ps、doctor、daemon start|stop|restart、connections list、apps list、monitor wait|ack、feedback send、tool\n\nUnified Runtime 開發預覽：`linkstart help tool` 說明共用工具入口；管理頁位於 daemon 的 /console。\n所有狀態輸出都會遮蔽 capability。使用 `linkstart help --json` 取得機器可讀說明。"),
    }
}
