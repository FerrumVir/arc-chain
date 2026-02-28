import { NextRequest, NextResponse } from "next/server";

const RPC_BASE = process.env.ARC_NODE_URL || "http://127.0.0.1:9090";
const TIMEOUT_MS = 3_000;

const corsHeaders = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type",
};

export async function OPTIONS(): Promise<NextResponse> {
  return NextResponse.json(null, { status: 204, headers: corsHeaders });
}

export async function GET(
  request: NextRequest,
  { params }: { params: Promise<{ path: string[] }> }
): Promise<NextResponse> {
  const { path } = await params;
  const rpcPath = path.join("/");
  const search = request.nextUrl.searchParams.toString();
  const url = `${RPC_BASE}/${rpcPath}${search ? `?${search}` : ""}`;

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), TIMEOUT_MS);

  try {
    const res = await fetch(url, { signal: controller.signal });
    clearTimeout(timer);

    // Handle empty responses (e.g. 404 with content-length: 0)
    const text = await res.text();
    if (!text || text.trim().length === 0) {
      return NextResponse.json(
        { error: `Node returned ${res.status} (empty body)`, _source: "unavailable" },
        { status: res.status >= 400 ? res.status : 503, headers: corsHeaders }
      );
    }

    let body: unknown;
    try {
      body = JSON.parse(text);
    } catch {
      return NextResponse.json(
        { error: "Invalid JSON from node", _source: "unavailable" },
        { status: 502, headers: corsHeaders }
      );
    }

    if (!res.ok) {
      // Forward 4xx/5xx from the RPC node with _source marker
      const errPayload = typeof body === "object" && body !== null
        ? { ...body, _source: "live" }
        : { error: text, _source: "live" };
      return NextResponse.json(
        errPayload,
        { status: res.status, headers: corsHeaders }
      );
    }

    // Wrap array responses so spread doesn't break them
    const payload = Array.isArray(body)
      ? { data: body, _source: "live" }
      : { ...(body as Record<string, unknown>), _source: "live" };
    return NextResponse.json(payload, { status: 200, headers: corsHeaders });
  } catch (err: unknown) {
    clearTimeout(timer);

    const isTimeout =
      err instanceof DOMException && err.name === "AbortError";
    const message = isTimeout ? "Node timeout" : "Node unavailable";

    return NextResponse.json(
      { error: message, _source: "unavailable" },
      { status: 503, headers: corsHeaders }
    );
  }
}
