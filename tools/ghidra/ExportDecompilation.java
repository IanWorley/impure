// Exports Ghidra's pseudo-C for every function in the current program.
// Output is derived from the original executable and must stay in the ignored local/ tree.
// @category pure-decomp
// @runtime Java

import java.io.IOException;
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;

import ghidra.app.decompiler.DecompInterface;
import ghidra.app.decompiler.DecompileOptions;
import ghidra.app.decompiler.DecompileResults;
import ghidra.app.decompiler.parallel.DecompilerCallback;
import ghidra.app.decompiler.parallel.ParallelDecompiler;
import ghidra.app.script.GhidraScript;
import ghidra.program.model.listing.Function;
import ghidra.util.task.TaskMonitor;

public class ExportDecompilation extends GhidraScript {

    private static final int DECOMPILE_TIMEOUT_SECONDS = 120;
    private static final String FUNCTIONS_DIRECTORY = "functions";
    private static final String INDEX_FILE = "index.tsv";
    private static final String FAILURES_FILE = "failures.tsv";

    private record ExportResult(String address, String name, long sizeBytes, String errorMessage) {
        boolean succeeded() {
            return errorMessage == null;
        }
    }

    @Override
    protected void run() throws Exception {
        String[] args = getScriptArgs();
        if (args.length != 1) {
            throw new IllegalArgumentException("Usage: ExportDecompilation.java <output-directory>");
        }

        Path outputDirectory = Path.of(args[0]);
        Path functionsDirectory = outputDirectory.resolve(FUNCTIONS_DIRECTORY);
        Files.createDirectories(functionsDirectory);

        List<Function> functions = new ArrayList<>();
        currentProgram.getFunctionManager().getFunctions(true).forEach(functions::add);
        println("Decompiling " + functions.size() + " functions");

        DecompilerCallback<ExportResult> callback =
            new DecompilerCallback<>(currentProgram, this::configureDecompiler) {
                @Override
                public ExportResult process(DecompileResults results, TaskMonitor taskMonitor)
                        throws IOException {
                    return writeFunction(functionsDirectory, results);
                }
            };
        callback.setTimeout(DECOMPILE_TIMEOUT_SECONDS);

        List<ExportResult> results;
        try {
            results = new ArrayList<>(ParallelDecompiler.decompileFunctions(callback, functions, monitor));
        }
        finally {
            callback.dispose();
        }
        results.sort(Comparator.comparing(ExportResult::address));

        writeIndex(outputDirectory, results);
        long failureCount = results.stream().filter(result -> !result.succeeded()).count();
        println("Exported " + (results.size() - failureCount) + " functions; " + failureCount
            + " failed (see " + FAILURES_FILE + ")");
    }

    private void configureDecompiler(DecompInterface decompiler) {
        DecompileOptions options = new DecompileOptions();
        options.grabFromProgram(currentProgram);
        decompiler.setOptions(options);
        decompiler.toggleCCode(true);
        decompiler.toggleSyntaxTree(true);
        decompiler.setSimplificationStyle("decompile");
    }

    private static ExportResult writeFunction(Path functionsDirectory, DecompileResults results)
            throws IOException {
        Function function = results.getFunction();
        String address = function.getEntryPoint().toString();
        long sizeBytes = function.getBody().getNumAddresses();

        if (!results.decompileCompleted() || results.getDecompiledFunction() == null) {
            String error = results.getErrorMessage();
            return new ExportResult(address, function.getName(), sizeBytes,
                error == null || error.isBlank() ? "decompilation did not complete" : error.strip());
        }

        Path file = functionsDirectory.resolve(address + ".c");
        String header = "// " + function.getName() + " @ " + address + " (" + sizeBytes
            + " bytes). Ghidra output: a hypothesis, not verified source.\n";
        Files.writeString(file, header + results.getDecompiledFunction().getC(),
            StandardCharsets.UTF_8);
        return new ExportResult(address, function.getName(), sizeBytes, null);
    }

    private static void writeIndex(Path outputDirectory, List<ExportResult> results)
            throws IOException {
        try (PrintWriter index = new PrintWriter(
                Files.newBufferedWriter(outputDirectory.resolve(INDEX_FILE), StandardCharsets.UTF_8));
             PrintWriter failures = new PrintWriter(
                Files.newBufferedWriter(outputDirectory.resolve(FAILURES_FILE), StandardCharsets.UTF_8))) {
            index.println("address\tname\tsize_bytes\tstatus");
            failures.println("address\tname\terror");
            for (ExportResult result : results) {
                index.println(String.join("\t", result.address(), result.name(),
                    Long.toString(result.sizeBytes()), result.succeeded() ? "ok" : "failed"));
                if (!result.succeeded()) {
                    failures.println(String.join("\t", result.address(), result.name(),
                        result.errorMessage().replaceAll("\\s+", " ")));
                }
            }
        }
    }
}
