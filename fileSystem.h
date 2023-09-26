#include "kernel.h"

#define FILES_MAX 2
#define DISK_MAX_SIZE align_up(sizeof(struct file) * FILES_MAX, SECTOR_SIZE)

struct tar_header {
    char name[100];
    char mode[8];
    char uid[8];
    char gid[8];
    char size[12];
    char mtime[12];
    char checksum[8];
    char type;
    char linkname[100];
    char magic[6];
    char version[2];
    char uname[32];
    char gname[32];
    char devmajor[8];
    char devminor[8];
    char prefix[155];
    char padding[12];
    char data[];  // ヘッダに続くデータ領域を指す配列 (フレキシブル配列メンバ)
} __attribute__((packed));

struct file {
    bool in_use;      // このファイルエントリが使われているか
    char name[100];   // ファイル名
    char data[1024];  // ファイルの内容
    size_t size;      // ファイルサイズ
};

extern void fs_init(void);
extern void fs_flush(void);
extern struct file *fs_lookup(const char *filename);
