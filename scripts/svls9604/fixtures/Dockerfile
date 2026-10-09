ARG RUNTIME=python
ARG AGENT_IMAGE

FROM python:3.12-slim AS python-plain
WORKDIR /app
RUN printf '%s\n' \
  'from http.server import BaseHTTPRequestHandler, HTTPServer' \
  'class H(BaseHTTPRequestHandler):' \
  '  def do_GET(self):' \
  '    self.send_response(200); self.end_headers(); self.wfile.write(b"Hello World!")' \
  'HTTPServer(("0.0.0.0", 8080), H).serve_forever()' > app.py
EXPOSE 8080
CMD ["python", "app.py"]

FROM node:22-slim AS node-plain
WORKDIR /app
RUN printf '%s\n' \
  "const http=require('http');" \
  "http.createServer((q,r)=>{r.writeHead(200);r.end('Hello World!')}).listen(8080,'0.0.0.0');" > app.js
EXPOSE 8080
CMD ["node", "app.js"]

FROM golang:1.24-bookworm AS go-build
WORKDIR /src
RUN printf '%s\n' \
  'package main' \
  'import ("fmt"; "net/http")' \
  'func main(){http.HandleFunc("/",func(w http.ResponseWriter,r *http.Request){fmt.Fprint(w,"Hello World!")});http.ListenAndServe(":8080",nil)}' > main.go \
  && go build -o /app main.go
FROM debian:bookworm-slim AS go-plain
COPY --from=go-build /app /app
EXPOSE 8080
CMD ["/app"]

FROM eclipse-temurin:21-jdk-jammy AS java-build
WORKDIR /src
RUN printf '%s\n' \
  'import com.sun.net.httpserver.HttpServer;' \
  'import java.net.InetSocketAddress;' \
  'public class App { public static void main(String[] a) throws Exception {' \
  'var s=HttpServer.create(new InetSocketAddress(8080),0); s.createContext("/",e->{byte[] b="Hello World!".getBytes();e.sendResponseHeaders(200,b.length);e.getResponseBody().write(b);e.close();});s.start();}}' > App.java \
  && javac App.java
FROM eclipse-temurin:21-jre-jammy AS java-plain
WORKDIR /app
COPY --from=java-build /src/App.class .
EXPOSE 8080
CMD ["java", "App"]

FROM mcr.microsoft.com/dotnet/sdk:8.0 AS dotnet-build
WORKDIR /src
RUN printf '%s\n' '<Project Sdk="Microsoft.NET.Sdk.Web"><PropertyGroup><TargetFramework>net8.0</TargetFramework><ImplicitUsings>enable</ImplicitUsings></PropertyGroup></Project>' > app.csproj \
  && printf '%s\n' 'var b=WebApplication.CreateBuilder(args);var a=b.Build();a.MapGet("/",()=>"Hello World!");a.Run("http://0.0.0.0:8080");' > Program.cs \
  && dotnet publish -c Release -o /out
FROM mcr.microsoft.com/dotnet/aspnet:8.0 AS dotnet-plain
WORKDIR /app
COPY --from=dotnet-build /out .
EXPOSE 8080
CMD ["dotnet", "app.dll"]

FROM ruby:3.3-slim AS ruby-plain
WORKDIR /app
RUN printf '%s\n' \
  'require "socket"' \
  's=TCPServer.new("0.0.0.0",8080)' \
  'loop{c=s.accept;c.gets;while (l=c.gets);break if l=="\r\n";end;c.write "HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nHello World!";c.close}' > app.rb
EXPOSE 8080
CMD ["ruby", "app.rb"]

FROM php:8.4-cli AS php-plain
WORKDIR /app
RUN printf '%s\n' '<?php echo "Hello World!"; ?>' > index.php
EXPOSE 8080
CMD ["php", "-S", "0.0.0.0:8080", "-t", "/app"]

FROM ${AGENT_IMAGE} AS agent
FROM ${RUNTIME}-plain AS plain

FROM python-plain AS python-init
COPY --from=agent /serverless-init /serverless-init
ENTRYPOINT ["/serverless-init"]
CMD ["python", "app.py"]

FROM node-plain AS node-init
COPY --from=agent /serverless-init /serverless-init
ENTRYPOINT ["/serverless-init"]
CMD ["node", "app.js"]

FROM go-plain AS go-init
COPY --from=agent /serverless-init /serverless-init
ENTRYPOINT ["/serverless-init"]
CMD ["/app"]

FROM java-plain AS java-init
COPY --from=agent /serverless-init /serverless-init
ENTRYPOINT ["/serverless-init"]
CMD ["java", "App"]

FROM dotnet-plain AS dotnet-init
COPY --from=agent /serverless-init /serverless-init
ENTRYPOINT ["/serverless-init"]
CMD ["dotnet", "app.dll"]

FROM ruby-plain AS ruby-init
COPY --from=agent /serverless-init /serverless-init
ENTRYPOINT ["/serverless-init"]
CMD ["ruby", "app.rb"]

FROM php-plain AS php-init
COPY --from=agent /serverless-init /serverless-init
ENTRYPOINT ["/serverless-init"]
CMD ["php", "-S", "0.0.0.0:8080", "-t", "/app"]
