// VeyraProbe — MQL4 WebRequest client for the Veyra EA control channel.
// MQL4 has no socket API; WebRequest is the terminal's native TCP/HTTP client.
// Places no orders: the order_check command only validates a request through
// the terminal's OrderCheck(), which never sends anything to the broker.
// Requires the endpoint to be listed in
// Tools -> Options -> Expert Advisors -> "Allow WebRequest for listed URL".
#property strict
#property version   "1.14"
#property description "Veyra control channel: heartbeat, account snapshot, and broker-side order validation. Places no orders."

input string InUrl         = "__VEYRA_URL__";   // Veyra endpoint (loopback or tunnel)
input string InToken       = "__VEYRA_TOKEN__"; // shared token
input int    InHeartbeatMs = 1000;              // heartbeat interval
input int    InTimeoutMs   = 1500;              // WebRequest timeout

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

// Extracts a JSON string value ("key":"value") from a compact response. The
// scan ignores nesting, so it also reads fields inside "order":{...}.
string JsonString(string source, string key)
  {
   string needle = "\"" + key + "\":\"";
   int start = StringFind(source, needle);
   if(start < 0) return("");
   start += StringLen(needle);
   int end = StringFind(source, "\"", start);
   if(end < 0) return("");
   return(StringSubstr(source, start, end - start));
  }

// Extracts a JSON number value ("key":123) from a compact response.
double JsonNumber(string source, string key)
  {
   string needle = "\"" + key + "\":";
   int start = StringFind(source, needle);
   if(start < 0) return(0.0);
   start += StringLen(needle);
   int len = StringLen(source);
   int end = start;
   while(end < len)
     {
      int ch = StringGetChar(source, end);
      if(ch == ',' || ch == '}' || ch == ']' || ch == ' ') break;
      end++;
     }
   return(StrToDouble(StringSubstr(source, start, end - start)));
  }

// Escapes a string for embedding in a JSON response body.
string EscapeJson(string value)
  {
   string out = value;
   StringReplace(out, "\\", "\\\\");
   StringReplace(out, "\"", "\\\"");
   StringReplace(out, "\n", " ");
   StringReplace(out, "\r", " ");
   return(out);
  }

void SendAck(string id, string dataJson)
  {
   string ack = "{\"t\":\"ack\",\"v\":1,\"token\":\"" + InToken + "\",\"id\":\"" + id + "\",";
   if(StringLen(dataJson) > 0)
      ack = ack + "\"ok\":true,\"data\":" + dataJson + "}";
   else
      ack = ack + "\"ok\":true}";
   string ack_response;
   int ack_status = PostJson(ack, ack_response);
   Print("VeyraProbe ack status=", ack_status);
  }

void SendAckError(string id, string reason)
  {
   string ack = "{\"t\":\"ack\",\"v\":1,\"token\":\"" + InToken + "\",\"id\":\"" + id
                + "\",\"ok\":false,\"error\":\"" + EscapeJson(reason) + "\"}";
   string ack_response;
   int ack_status = PostJson(ack, ack_response);
   Print("VeyraProbe ack error=", reason, " status=", ack_status);
  }

// Classic-MQL4 validation for one order request. This terminal build exposes
// no MQL5-style OrderCheck, so the EA applies the terminal's own market rules
// (volume range and step, stop distance, price side) plus its margin engine
// through AccountFreeMarginCheck. Nothing is sent to the broker.
void HandleOrderCheck(string response, string id)
  {
   string symbol    = JsonString(response, "symbol");
   string side      = JsonString(response, "side");
   string orderType = JsonString(response, "order_type");
   double price     = JsonNumber(response, "price");
   double volume    = JsonNumber(response, "volume");
   double sl        = JsonNumber(response, "stop_loss");
   double tp        = JsonNumber(response, "take_profit");

   if(StringLen(symbol) == 0 || volume <= 0.0)
     {
      SendAckError(id, "malformed order_check request");
      return;
     }

   int code = 0;
   string comment = "ok";

   double minLot    = MarketInfo(symbol, MODE_MINLOT);
   double maxLot    = MarketInfo(symbol, MODE_MAXLOT);
   double lotStep   = MarketInfo(symbol, MODE_LOTSTEP);
   double stopLevel = MarketInfo(symbol, MODE_STOPLEVEL);
   double point     = MarketInfo(symbol, MODE_POINT);
   double ask       = MarketInfo(symbol, MODE_ASK);
   double bid       = MarketInfo(symbol, MODE_BID);
   double marginRequired = MarketInfo(symbol, MODE_MARGINREQUIRED) * volume;

   if(!IsConnected())
     {
      code = 6;
      comment = "terminal is not connected";
     }
   else if(minLot <= 0.0)
     {
      code = 133;
      comment = "symbol unknown or not tradable";
     }
   else if(!IsTradeAllowed() || MarketInfo(symbol, MODE_TRADEALLOWED) != 1.0)
     {
      code = 133;
      comment = "trading disabled for this terminal or symbol";
     }
   else if(volume < minLot || volume > maxLot)
     {
      code = 131;
      comment = "volume outside the allowed range";
     }
   else if(lotStep > 0.0 && MathAbs(volume / lotStep - MathRound(volume / lotStep)) > 0.001)
     {
      code = 131;
      comment = "volume is not a multiple of the lot step";
     }
   else
     {
      double entry = price;
      if(orderType == "market")
        {
         if(ask <= 0.0 || bid <= 0.0)
           {
            code = 136;
            comment = "no quotes available for this symbol";
           }
         else if(side == "buy") entry = ask;
         else                   entry = bid;
        }
      else if(orderType == "limit")
        {
         if(price <= 0.0 || (side == "buy" && price >= ask) || (side == "sell" && price <= bid))
           {
            code = 129;
            comment = "limit price is on the wrong side of the market";
           }
        }
      else if(orderType == "stop")
        {
         if(price <= 0.0 || (side == "buy" && price <= ask) || (side == "sell" && price >= bid))
           {
            code = 129;
            comment = "stop price is on the wrong side of the market";
           }
        }
      else
        {
         code = 129;
         comment = "unknown order_type";
        }

      if(code == 0 && stopLevel > 0.0 && point > 0.0)
        {
         if(sl > 0.0)
           {
            bool wrongSide = (side == "buy" ? sl >= entry : sl <= entry);
            if(wrongSide || MathAbs(entry - sl) / point < stopLevel)
              {
               code = 130;
               comment = "stop loss violates the minimum stop distance";
              }
           }
         if(code == 0 && tp > 0.0)
           {
            bool wrongSide = (side == "buy" ? tp <= entry : tp >= entry);
            if(wrongSide || MathAbs(tp - entry) / point < stopLevel)
              {
               code = 130;
               comment = "take profit violates the minimum stop distance";
              }
           }
        }
     }

   if(code == 0)
     {
      int cmd = OP_SELL;
      if(side == "buy") cmd = OP_BUY;
      ResetLastError();
      double freeAfter = AccountFreeMarginCheck(symbol, cmd, volume);
      int marginError = GetLastError();
      if(freeAfter <= 0.0 || marginError == 134)
        {
         code = 134;
         comment = "not enough free margin for this order";
        }
     }

   bool passed = (code == 0);
   string data = "{\"passed\":" + (passed ? "true" : "false")
                 + ",\"retcode\":" + (string)code
                 + ",\"comment\":\"" + EscapeJson(comment) + "\""
                 + ",\"margin\":" + DoubleToString(marginRequired, 2) + "}";
   Print("VeyraProbe order_check passed=", passed, " retcode=", (string)code,
         " comment=", comment, " margin=", DoubleToString(marginRequired, 2));
   SendAck(id, data);
  }

// Executes one command delivered by the service and acknowledges it by id.
void HandleCommand(string response)
  {
   string id = JsonString(response, "id");
   string kind = JsonString(response, "kind");
   if(StringLen(id) == 0) return;

   if(kind == "order_check")
     {
      HandleOrderCheck(response, id);
      return;
     }

   string data = "";
   if(kind == "ping")
     {
      // No payload.
     }
   else if(kind == "account_snapshot")
     {
      data = "{\"balance\":" + DoubleToString(AccountBalance(), 2)
             + ",\"equity\":" + DoubleToString(AccountEquity(), 2)
             + ",\"freeMargin\":" + DoubleToString(AccountFreeMargin(), 2)
             + ",\"orders\":" + (string)OrdersTotal()
             + ",\"serverTime\":" + (string)(long)TimeLocal() + "}";
     }
   else
     {
      SendAckError(id, "unsupported command");
      return;
     }
   SendAck(id, data);
  }

// Handles a service reply: commands take precedence, then the ping handshake.
void HandleResponse(string response)
  {
   if(StringFind(response, "\"t\":\"cmd\"") >= 0)
     {
      HandleCommand(response);
      return;
     }
   if(StringFind(response, "\"ping\"") >= 0)
     {
      string pong = "{\"t\":\"pong\",\"token\":\"" + InToken + "\",\"ts\":" + (string)(long)TimeLocal() + "}";
      string pong_response;
      int pong_status = PostJson(pong, pong_response);
      Print("VeyraProbe pong status=", pong_status);
     }
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
                 + ",\"orders\":" + (string)OrdersTotal()
                 + ",\"balance\":" + DoubleToString(AccountBalance(), 2)
                 + ",\"ts\":" + (string)(long)TimeLocal() + "}";

   string response;
   int status = PostJson(body, response);
   if(status == -1) return;
   g_said_hello = true;
   Print("VeyraProbe http ", status, " rx=", response);

   HandleResponse(response);
  }
