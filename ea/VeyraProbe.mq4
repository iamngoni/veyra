// VeyraProbe — MQL4 WebRequest probe for the Veyra EA control channel.
// MQL4 has no socket API; WebRequest is the terminal's native TCP/HTTP client.
// Places no trades and touches no orders. Requires the endpoint to be listed
// in Tools -> Options -> Expert Advisors -> "Allow WebRequest for listed URL".
#property strict
#property version   "1.12"
#property description "Veyra control-channel probe over localhost HTTP. No trading logic."

input string InUrl     = "__VEYRA_URL__";    // Veyra endpoint (loopback or tunnel)
input string InToken       = "__VEYRA_TOKEN__";                          // shared token
input int    InHeartbeatMs = 1000;                             // heartbeat interval
input int    InTimeoutMs   = 1500;                             // WebRequest timeout

uint g_last       = 0;
bool g_said_hello = false;

int OnInit()
  {
   EventSetMillisecondTimer(250);
   Print("VeyraProbe init -> ", InUrl);
   return(INIT_SUCCEEDED);
  }

void OnDeinit(const int reason)
  {
   EventKillTimer();
   Print("VeyraProbe stopped reason=", reason);
  }

int PostJson(string body, string &response)
  {
   // Build the byte array ourselves: StringToCharArray's count/codepage
   // semantics vary between builds, which produced malformed request bodies.
   int len = StringLen(body);
   char data[];
   ArrayResize(data, len);
   for(int i = 0; i < len; i++)
      data[i] = (char)(StringGetChar(body, i) & 0xFF);
   char result[];
   string headers;
   ResetLastError();
   int status = WebRequest("POST", InUrl, "", "", InTimeoutMs, data, len, result, headers);
   if(status == -1)
     {
      response = "";
      Print("VeyraProbe webrequest error=", GetLastError());
      return -1;
     }
   response = CharArrayToString(result, 0, ArraySize(result));
   return status;
  }

void OnTimer()
  {
   if(GetTickCount() - g_last < (uint)InHeartbeatMs) return;
   g_last = GetTickCount();

   string kind = (g_said_hello ? "hb" : "hello");
   string body = "{\"t\":\"" + kind + "\",\"v\":1,\"token\":\"" + InToken + "\""
                 + ",\"acct\":" + (string)AccountNumber()
                 + ",\"server\":\"" + AccountServer() + "\""
                 + ",\"symbol\":\"" + Symbol() + "\""
                 + ",\"connected\":" + (IsConnected() ? "true" : "false")
                 + ",\"tradeAllowed\":" + (IsTradeAllowed() ? "true" : "false")
                 + ",\"balance\":" + DoubleToString(AccountBalance(), 2)
                 + ",\"ts\":" + (string)(long)TimeLocal() + "}";

   string response;
   int status = PostJson(body, response);
   if(status == -1) return;
   g_said_hello = true;
   Print("VeyraProbe http ", status, " rx=", response);

   if(StringFind(response, "\"ping\"") >= 0)
     {
      string pong = "{\"t\":\"pong\",\"token\":\"" + InToken + "\",\"ts\":" + (string)(long)TimeLocal() + "}";
      string pong_response;
      int pong_status = PostJson(pong, pong_response);
      Print("VeyraProbe pong status=", pong_status);
     }
  }
