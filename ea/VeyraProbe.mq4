// VeyraProbe — MQL4 WebRequest client for the Veyra EA control channel.
// MQL4 has no socket API; WebRequest is the terminal's native TCP/HTTP client.
// Validates orders with the terminal's own market rules and margin engine
// (order_check) and, only when explicitly armed with InAllowLiveOrders, places
// live orders on an approved service command. Disarmed it reports a dry run.
// Requires the endpoint to be listed in
// Tools -> Options -> Expert Advisors -> "Allow WebRequest for listed URL".
#property strict
#property version   "1.17"
#property description "Veyra control channel: heartbeat, account/position snapshots, broker-side order validation, gated live order execution, and Veyra-owned position closes."

input string InUrl         = "__VEYRA_URL__";   // Veyra endpoint (loopback or tunnel)
input string InToken       = "__VEYRA_TOKEN__"; // shared token
input int    InHeartbeatMs = 1000;              // heartbeat interval
input int    InTimeoutMs   = 1500;              // WebRequest timeout
input bool   InAllowLiveOrders = false;         // arm live order placement (dry run when false)

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

// Builds the typed execution result the service validates for open_order and
// close_order acknowledgements.
string ExecutionResultJson(bool executed, int code, string comment, int ticket, double price, int digits)
  {
   return("{\"executed\":" + (executed ? "true" : "false")
          + ",\"retcode\":" + (string)code
          + ",\"comment\":\"" + EscapeJson(comment) + "\""
          + ",\"ticket\":" + (string)ticket
          + ",\"price\":" + DoubleToString(price, digits) + "}");
  }

// Validates one order request against the terminal's market rules and margin
// engine: volume range and step, price side, stop distance, and free margin.
// Returns 0 when the request would be accepted, otherwise a classic MT4 trade
// code, with a human explanation and the required margin. A negative return
// means the request itself was malformed.
int ValidateOrderRequest(string response, string &comment, double &margin, double &entryPrice)
  {
   string symbol    = JsonString(response, "symbol");
   string side      = JsonString(response, "side");
   string orderType = JsonString(response, "order_type");
   double price     = JsonNumber(response, "price");
   double volume    = JsonNumber(response, "volume");
   double sl        = JsonNumber(response, "stop_loss");
   double tp        = JsonNumber(response, "take_profit");

   comment = "ok";
   margin = 0.0;
   entryPrice = 0.0;

   if(StringLen(symbol) == 0 || volume <= 0.0)
     {
      comment = "malformed order request";
      return(-1);
     }

   int code = 0;
   double minLot    = MarketInfo(symbol, MODE_MINLOT);
   double maxLot    = MarketInfo(symbol, MODE_MAXLOT);
   double lotStep   = MarketInfo(symbol, MODE_LOTSTEP);
   double stopLevel = MarketInfo(symbol, MODE_STOPLEVEL);
   double point     = MarketInfo(symbol, MODE_POINT);
   double ask       = MarketInfo(symbol, MODE_ASK);
   double bid       = MarketInfo(symbol, MODE_BID);
   margin = MarketInfo(symbol, MODE_MARGINREQUIRED) * volume;

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
      entryPrice = price;
      if(orderType == "market")
        {
         if(ask <= 0.0 || bid <= 0.0)
           {
            code = 136;
            comment = "no quotes available for this symbol";
           }
         else if(side == "buy") entryPrice = ask;
         else                   entryPrice = bid;
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
            bool wrongSide = (side == "buy" ? sl >= entryPrice : sl <= entryPrice);
            if(wrongSide || MathAbs(entryPrice - sl) / point < stopLevel)
              {
               code = 130;
               comment = "stop loss violates the minimum stop distance";
              }
           }
         if(code == 0 && tp > 0.0)
           {
            bool wrongSide = (side == "buy" ? tp <= entryPrice : tp >= entryPrice);
            if(wrongSide || MathAbs(tp - entryPrice) / point < stopLevel)
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

   return(code);
  }

// Reports the terminal verdict for an order_check. The request is validated
// and the outcome echoed; nothing is ever sent to the broker.
void HandleOrderCheck(string response, string id)
  {
   string comment = "";
   double margin = 0.0;
   double entry = 0.0;
   int code = ValidateOrderRequest(response, comment, margin, entry);
   if(code < 0)
     {
      SendAckError(id, comment);
      return;
     }

   bool passed = (code == 0);
   string data = "{\"passed\":" + (passed ? "true" : "false")
                 + ",\"retcode\":" + (string)code
                 + ",\"comment\":\"" + EscapeJson(comment) + "\""
                 + ",\"margin\":" + DoubleToString(margin, 2) + "}";
   Print("VeyraProbe order_check passed=", passed, " retcode=", (string)code, " comment=", comment,
         " margin=", DoubleToString(margin, 2));
   SendAck(id, data);
  }

// Executes one live order when the terminal is armed; otherwise validates the
// request and reports a dry run. The OrderSend branch is compiled but only
// reachable when the operator recompiles with InAllowLiveOrders = true, so
// real money needs the service switch, a gate approval, and this input.
void HandleOpenOrder(string response, string id)
  {
   string comment = "";
   double margin = 0.0;
   double entry = 0.0;
   int code = ValidateOrderRequest(response, comment, margin, entry);
   if(code < 0)
     {
      SendAckError(id, comment);
      return;
     }

   if(code != 0 || !InAllowLiveOrders)
     {
      if(code == 0) comment = "dry run (live orders disabled in EA)";
      Print("VeyraProbe open_order dry run code=", (string)code, " comment=", comment);
      SendAck(id, ExecutionResultJson(false, code, comment, 0, 0.0, 2));
      return;
     }

   string symbol    = JsonString(response, "symbol");
   string side      = JsonString(response, "side");
   string orderType = JsonString(response, "order_type");
   double volume    = JsonNumber(response, "volume");
   double sl        = JsonNumber(response, "stop_loss");
   double tp        = JsonNumber(response, "take_profit");
   int magic        = (int)JsonNumber(response, "magic");

   // Classic MQL4 order-type values; this build declares only OP_BUY/OP_SELL.
   int cmd = 1;
   if(side == "buy") cmd = 0;
   if(orderType == "limit")
     {
      if(side == "buy") cmd = 2;
      else              cmd = 3;
     }
   else if(orderType == "stop")
     {
      if(side == "buy") cmd = 4;
      else              cmd = 5;
     }

   ResetLastError();
   int ticket = OrderSend(symbol, cmd, volume, entry, 10, sl, tp, "Veyra", magic, 0, CLR_NONE);
   int sendError = GetLastError();
   if(ticket <= 0)
     {
      Print("VeyraProbe open_order failed error=", (string)sendError);
      SendAck(id, ExecutionResultJson(false, sendError, "order send failed", 0, 0.0, 2));
      return;
     }

   double fillPrice = entry;
   if(OrderSelect(ticket, SELECT_BY_TICKET)) fillPrice = OrderOpenPrice();
   int digits = (int)MarketInfo(symbol, MODE_DIGITS);
   if(digits <= 0) digits = 5;
   Print("VeyraProbe open_order sent ticket=", (string)ticket, " price=",
         DoubleToString(fillPrice, digits));
   SendAck(id, ExecutionResultJson(true, 0, "order sent", ticket, fillPrice, digits));
  }

// Total open volume in lots across every open order.
double OpenLots()
  {
   double total = 0.0;
   for(int i = 0; i < OrdersTotal(); i++)
     {
      if(OrderSelect(i, SELECT_BY_POS, MODE_TRADES)) total += OrderLots();
     }
   return(total);
  }

// Stable wire names for terminal order kinds. This MQL4 build declares only
// OP_BUY and OP_SELL, so the remaining documented order-type values are
// matched explicitly.
string OrderKindName(int type)
  {
   switch(type)
     {
      case 0: return("buy");
      case 1: return("sell");
      case 2: return("buy_limit");
      case 3: return("sell_limit");
      case 4: return("buy_stop");
      case 5: return("sell_stop");
      case 6: return("buy_stop_limit");
      case 7: return("sell_stop_limit");
     }
   return("unknown");
  }

// Bounded JSON array of open orders for the account snapshot. OrderSelect()
// moves the terminal's selection cursor, so callers must not depend on it.
string PositionsJson(int maxEntries)
  {
   string out = "[";
   int included = 0;
   int total = OrdersTotal();
   for(int i = 0; i < total && included < maxEntries; i++)
     {
      if(!OrderSelect(i, SELECT_BY_POS, MODE_TRADES)) continue;
      int digits = (int)MarketInfo(OrderSymbol(), MODE_DIGITS);
      if(digits <= 0) digits = 5;
      if(included > 0) out = out + ",";
      out = out + "{\"ticket\":" + (string)OrderTicket()
            + ",\"symbol\":\"" + EscapeJson(OrderSymbol()) + "\""
            + ",\"kind\":\"" + OrderKindName(OrderType()) + "\""
            + ",\"magic\":" + (string)(int)OrderMagicNumber()
            + ",\"lots\":" + DoubleToString(OrderLots(), 2)
            + ",\"price\":" + DoubleToString(OrderOpenPrice(), digits)
            + ",\"profit\":" + DoubleToString(OrderProfit(), 2) + "}";
      included++;
     }
   return(out + "]");
  }

// Closes one Veyra-owned market position when the terminal is armed; otherwise
// validates the ticket and reports a dry run. Pending orders are not touched.
void HandleCloseOrder(string response, string id)
  {
   int ticket = (int)JsonNumber(response, "ticket");
   int magic  = (int)JsonNumber(response, "magic");

   if(ticket <= 0)
     {
      SendAckError(id, "malformed close request");
      return;
     }

   if(!OrderSelect(ticket, SELECT_BY_TICKET))
     {
      SendAck(id, ExecutionResultJson(false, 4108, "unknown ticket", 0, 0.0, 2));
      return;
     }
   if((int)OrderMagicNumber() != magic)
     {
      SendAck(id, ExecutionResultJson(false, 4108, "ticket is not a Veyra position", 0, 0.0, 2));
      return;
     }
   int type = OrderType();
   if(type != OP_BUY && type != OP_SELL)
     {
      SendAck(id, ExecutionResultJson(false, 4108, "not an open market position", 0, 0.0, 2));
      return;
     }
   string symbol = OrderSymbol();
   double lots = OrderLots();
   double price = MarketInfo(symbol, MODE_BID);
   if(type == OP_SELL) price = MarketInfo(symbol, MODE_ASK);
   if(lots <= 0.0 || price <= 0.0)
     {
      SendAck(id, ExecutionResultJson(false, 4108, "position has no closable volume or quotes", 0, 0.0, 2));
      return;
     }

   if(!InAllowLiveOrders)
     {
      Print("VeyraProbe close_order dry run ticket=", (string)ticket);
      SendAck(id, ExecutionResultJson(false, 0, "dry run (live orders disabled in EA)", 0, 0.0, 2));
      return;
     }

   ResetLastError();
   bool closed = OrderClose(ticket, lots, price, 10, CLR_NONE);
   int closeError = GetLastError();
   if(!closed)
     {
      Print("VeyraProbe close_order failed ticket=", (string)ticket, " error=", (string)closeError);
      SendAck(id, ExecutionResultJson(false, closeError, "close failed", 0, 0.0, 2));
      return;
     }

   int digits = (int)MarketInfo(symbol, MODE_DIGITS);
   if(digits <= 0) digits = 5;
   Print("VeyraProbe close_order closed ticket=", (string)ticket, " price=", DoubleToString(price, digits));
   SendAck(id, ExecutionResultJson(true, 0, "closed", ticket, price, digits));
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

   if(kind == "open_order")
     {
      HandleOpenOrder(response, id);
      return;
     }

   if(kind == "close_order")
     {
      HandleCloseOrder(response, id);
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
             + ",\"lots\":" + DoubleToString(OpenLots(), 2)
             + ",\"positions\":" + PositionsJson(32)
             + ",\"positionsTruncated\":" + (OrdersTotal() > 32 ? "true" : "false")
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
                 + ",\"lots\":" + DoubleToString(OpenLots(), 2)
                 + ",\"balance\":" + DoubleToString(AccountBalance(), 2)
                 + ",\"ts\":" + (string)(long)TimeLocal() + "}";

   string response;
   int status = PostJson(body, response);
   if(status == -1) return;
   g_said_hello = true;
   Print("VeyraProbe http ", status, " rx=", response);

   HandleResponse(response);
  }
