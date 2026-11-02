// Headless Ghidra query server for reverse-engineering noita.exe from the command line.
// Run: analyzeHeadless <projdir> NoitaProj -process noita.exe -noanalysis -readOnly \
//        -scriptPath tools/ghidra -postScript QueryServer.java [port]
// Endpoints (GET, plain text): /decompile?addr=  /disasm?addr=&n=  /xrefs?addr=
//   /strings?q=  /func?addr=  /bytes?addr=&n=  /search?hex=  /callees?addr=  /quit
//@category NoitaOnline
import ghidra.app.script.GhidraScript;
import ghidra.app.decompiler.*;
import ghidra.program.model.listing.*;
import ghidra.program.model.address.*;
import ghidra.program.model.symbol.*;
import ghidra.program.model.mem.*;
import ghidra.program.util.DefinedStringIterator;
import ghidra.program.model.data.StringDataInstance;
import com.sun.net.httpserver.*;
import java.net.*;
import java.io.*;
import java.util.*;
import java.util.concurrent.CountDownLatch;

public class QueryServer extends GhidraScript {
    private DecompInterface decomp;
    private final CountDownLatch done = new CountDownLatch(1);

    private Map<String, String> params(URI uri) {
        Map<String, String> m = new HashMap<>();
        String q = uri.getRawQuery();
        if (q == null) return m;
        for (String kv : q.split("&")) {
            int i = kv.indexOf('=');
            try {
                if (i > 0) m.put(kv.substring(0, i), URLDecoder.decode(kv.substring(i + 1), "UTF-8"));
            } catch (Exception e) { }
        }
        return m;
    }

    private Address addr(String s) {
        return currentProgram.getAddressFactory().getDefaultAddressSpace().getAddress(Long.decode(s));
    }

    private synchronized String handle(String path, Map<String, String> p) throws Exception {
        StringBuilder sb = new StringBuilder();
        Listing listing = currentProgram.getListing();
        switch (path) {
            case "/decompile": {
                Function f = getFunctionContaining(addr(p.get("addr")));
                if (f == null) return "no function\n";
                DecompileResults r = decomp.decompileFunction(f, 120, monitor);
                if (!r.decompileCompleted()) return "decompile failed: " + r.getErrorMessage() + "\n";
                return "// " + f.getName() + " @ " + f.getEntryPoint() + "\n" + r.getDecompiledFunction().getC();
            }
            case "/disasm": {
                Address a = addr(p.get("addr"));
                int n = Integer.parseInt(p.getOrDefault("n", "40"));
                Instruction ins = listing.getInstructionAt(a);
                if (ins == null) ins = listing.getInstructionAfter(a);
                for (int i = 0; i < n && ins != null; i++, ins = ins.getNext()) {
                    sb.append(ins.getAddress()).append("  ").append(ins.toString()).append("\n");
                }
                return sb.toString();
            }
            case "/xrefs": {
                Address a = addr(p.get("addr"));
                for (Reference r : getReferencesTo(a)) {
                    Function f = getFunctionContaining(r.getFromAddress());
                    sb.append(r.getFromAddress()).append(" ").append(r.getReferenceType())
                      .append(" in ").append(f == null ? "?" : f.getName() + "@" + f.getEntryPoint()).append("\n");
                }
                return sb.toString();
            }
            case "/strings": {
                String q = p.get("q");
                int limit = Integer.parseInt(p.getOrDefault("limit", "200"));
                int n = 0;
                for (Data d : DefinedStringIterator.forProgram(currentProgram)) {
                    StringDataInstance s = StringDataInstance.getStringDataInstance(d);
                    String v = s.getStringValue();
                    if (v != null && v.contains(q)) {
                        sb.append(d.getAddress()).append(" ").append(v.replace("\n", "\\n")).append("\n");
                        if (++n >= limit) break;
                    }
                }
                return sb.toString();
            }
            case "/func": {
                Function f = getFunctionContaining(addr(p.get("addr")));
                if (f == null) return "none\n";
                return f.getName() + " " + f.getEntryPoint() + " " + f.getBody().getMinAddress() + "-" + f.getBody().getMaxAddress() + " " + f.getSignature() + "\n";
            }
            case "/callees": {
                Function f = getFunctionContaining(addr(p.get("addr")));
                if (f == null) return "none\n";
                for (Function c : f.getCalledFunctions(monitor)) sb.append(c.getEntryPoint()).append(" ").append(c.getName()).append("\n");
                return sb.toString();
            }
            case "/callers": {
                Function f = getFunctionContaining(addr(p.get("addr")));
                if (f == null) return "none\n";
                for (Function c : f.getCallingFunctions(monitor)) sb.append(c.getEntryPoint()).append(" ").append(c.getName()).append("\n");
                return sb.toString();
            }
            case "/bytes": {
                Address a = addr(p.get("addr"));
                int n = Integer.parseInt(p.getOrDefault("n", "64"));
                byte[] b = new byte[n];
                currentProgram.getMemory().getBytes(a, b);
                for (int i = 0; i < n; i++) sb.append(String.format("%02x", b[i] & 0xff)).append(i % 16 == 15 ? "\n" : " ");
                return sb.append("\n").toString();
            }
            case "/search": {
                String hex = p.get("hex").replace(" ", "");
                byte[] pat = new byte[hex.length() / 2];
                byte[] mask = new byte[hex.length() / 2];
                for (int i = 0; i < pat.length; i++) {
                    String h = hex.substring(2 * i, 2 * i + 2);
                    if (h.equals("??")) { pat[i] = 0; mask[i] = 0; } else { pat[i] = (byte) Integer.parseInt(h, 16); mask[i] = (byte) 0xff; }
                }
                Memory mem = currentProgram.getMemory();
                Address start = currentProgram.getMinAddress();
                int n = 0;
                while (start != null && n < 50) {
                    Address found = mem.findBytes(start, pat, mask, true, monitor);
                    if (found == null) break;
                    Function f = getFunctionContaining(found);
                    sb.append(found).append(" in ").append(f == null ? "?" : f.getName()).append("\n");
                    start = found.add(1);
                    n++;
                }
                return sb.toString();
            }
            case "/quit":
                done.countDown();
                return "bye\n";
        }
        return "unknown endpoint\n";
    }

    @Override
    protected void run() throws Exception {
        int port = 18489;
        String[] args = getScriptArgs();
        if (args.length > 0) port = Integer.parseInt(args[0]);
        decomp = new DecompInterface();
        decomp.openProgram(currentProgram);
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", port), 0);
        server.createContext("/", ex -> {
            String out;
            try {
                out = handle(ex.getRequestURI().getPath(), params(ex.getRequestURI()));
            } catch (Exception e) {
                StringWriter sw = new StringWriter();
                e.printStackTrace(new PrintWriter(sw));
                out = "error: " + sw;
            }
            byte[] b = out.getBytes("UTF-8");
            ex.sendResponseHeaders(200, b.length);
            ex.getResponseBody().write(b);
            ex.close();
        });
        server.start();
        println("QueryServer listening on " + port);
        done.await();
        server.stop(0);
    }
}
