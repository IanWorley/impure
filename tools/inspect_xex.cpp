#include <disasm.h>
#include <image.h>

#include <algorithm>
#include <array>
#include <charconv>
#include <cstdint>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <iterator>
#include <stdexcept>
#include <string_view>
#include <vector>

namespace {
constexpr std::size_t kInstructionBytes = sizeof(std::uint32_t);
constexpr std::size_t kDefaultInstructionCount = 64;
constexpr std::array<char, 4> kXexMagic{'X', 'E', 'X', '2'};
constexpr std::size_t kMinimumHeaderBytes = 24;
constexpr std::size_t kPeSectionNameBytes = 8;
constexpr int kHexBase = 16;
constexpr int kDecimalBase = 10;

std::size_t ParseNumber(std::string_view text) {
    const bool hexadecimal = text.starts_with("0x");
    if (hexadecimal) {
        text.remove_prefix(2);
    }
    std::size_t value{};
    const auto [end, error] = std::from_chars(
        text.data(), text.data() + text.size(), value,
        hexadecimal ? kHexBase : kDecimalBase);
    if (error != std::errc{} || end != text.data() + text.size()) {
        throw std::runtime_error("Invalid numeric argument");
    }
    return value;
}
}

int main(int argc, char** argv) {
    constexpr int kMinimumArguments = 3;
    constexpr int kMaximumArguments = 5;
    if (argc < kMinimumArguments || argc > kMaximumArguments) {
        std::cerr << "Usage: inspect_xex <XEX> <new output directory> [address] [instruction count]\n";
        return 1;
    }

    try {
        const std::filesystem::path outputDirectory(argv[2]);
        if (std::filesystem::exists(outputDirectory)) {
            throw std::runtime_error("Output directory already exists; choose a new directory");
        }
        std::ifstream input(argv[1], std::ios::binary);
        if (!input) {
            throw std::runtime_error("Cannot open input executable");
        }
        const std::vector<std::uint8_t> bytes{
            std::istreambuf_iterator<char>(input), std::istreambuf_iterator<char>()};
        if (bytes.size() < kMinimumHeaderBytes ||
            !std::equal(kXexMagic.begin(), kXexMagic.end(), bytes.begin())) {
            throw std::runtime_error("Input is not an XEX2 executable");
        }

        // Use upstream's retail decryption/decompression and Xenon disassembler.
        // Its loader patches import thunks, so the exported image is for analysis.
        auto image = Image::ParseImage(bytes.data(), bytes.size());
        if (!image.data || image.size == 0 || image.sections.empty()) {
            throw std::runtime_error("XenonUtils could not load the executable");
        }
        const auto address = argc > 3 ? ParseNumber(argv[3]) : image.entry_point;
        const auto count = argc > 4 ? ParseNumber(argv[4]) : kDefaultInstructionCount;
        if (address % kInstructionBytes != 0) {
            throw std::runtime_error("Instruction address must be aligned");
        }
        const Section* selected = nullptr;
        for (const auto& section : image.sections) {
            if (address >= section.base && address - section.base < section.size &&
                (section.flags & SectionFlags_Code)) {
                selected = &section;
                break;
            }
        }
        if (!selected || count > (selected->size - (address - selected->base)) / kInstructionBytes) {
            throw std::runtime_error("Requested instructions are outside a code section");
        }

        if (!std::filesystem::create_directory(outputDirectory)) {
            throw std::runtime_error("Output directory already exists; choose a new directory");
        }
        std::ofstream mapped(outputDirectory / "analysis-image.bin", std::ios::binary);
        mapped.write(reinterpret_cast<const char*>(image.data.get()), image.size);
        if (!mapped) {
            throw std::runtime_error("Cannot write analysis image");
        }
        std::cout << std::hex << "Image base: 0x" << image.base
                  << "\nEntry point: 0x" << image.entry_point
                  << "\nImage size: 0x" << image.size << '\n';
        for (const auto& section : image.sections) {
            std::cout << "Section " << section.name.substr(0, kPeSectionNameBytes) << ": 0x" << section.base
                      << " size 0x" << section.size
                      << ((section.flags & SectionFlags_Code) ? " code" : " data") << '\n';
        }
        for (const auto& symbol : image.symbols) {
            std::cout << "Import 0x" << symbol.address << ' ' << symbol.name << '\n';
        }
        std::cout << "\nDisassembly:\n";
        for (std::size_t index = 0; index < count; ++index) {
            const auto current = address + index * kInstructionBytes;
            ppc_insn instruction{};
            ppc::Disassemble(selected->data + (current - selected->base), current, instruction);
            std::cout << "0x" << current << ": "
                      << (instruction.opcode ? instruction.opcode->name : "unknown")
                      << ' ' << instruction.op_str << '\n';
        }
    } catch (const std::exception& error) {
        std::cerr << error.what() << '\n';
        return 1;
    }
}
